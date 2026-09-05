//! Live MCP server connections for the chat session.
//!
//! [`Mcp`] holds the connections opened once at session start (see
//! [`crate::run_chat`]) and reused for every tool call. It bridges the discovery
//! ([`newt_core::mcp`]) and client ([`newt_mcp_client`]) layers into the TUI's
//! agent loop: it advertises the remote tools (namespaced `server__tool`) in the
//! tool list, and routes a namespaced call to the right server.
//!
//! It connects **stdio** and **streamable-HTTP** servers. A spawned **stdio**
//! server now runs *inside* the session's Caveats leash (#1243 Leg 3): its
//! process is confined by [`agent_bridle::ConfinedCommand`] to the same
//! authority as a `run_command`, instead of running ambient with the host's
//! full authority. (Remote **HTTP** tools still run with whatever authority
//! their own server has; only their egress host is net-gated, #1156.)
//!
//! Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 15:53 EDT | Date: 2026-08-12

use newt_core::mcp::{McpServerEntry, TransportKind};
use newt_mcp_client::{
    connect_http_with_runtime_bearer, connect_stdio, openai_tool_definition, split_namespaced,
    ConnectedServer as ClientConnectedServer,
};
use serde_json::Value;

/// Per-server launch outcome for the `/mcp` surface (#1149).
#[derive(Debug, Clone)]
pub(crate) enum McpStatus {
    /// Connected, with this many tools registered and the confinement (#1243
    /// Leg 3) + network-egress (Leg 4) postures achieved.
    Connected {
        tools: usize,
        confinement: Confinement,
        net: NetGate,
    },
    /// Skipped at launch (auth failure, timeout, spawn error, legacy SSE…).
    Skipped(String),
    /// `enabled = false` in config — not attempted.
    Disabled,
}

/// The local confinement posture of a connected server, for the `/mcp` table
/// (#1243 Leg 3). A spawned stdio server runs inside the session's OCAP boundary;
/// a remote HTTP server has no local process to confine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Confinement {
    /// A spawned stdio server confined by a kernel OS sandbox — the achieved
    /// `SandboxKind` name (e.g. `Landlock`, `Seatbelt`).
    Confined(String),
    /// A spawned stdio server that ran through the `ConfinedCommand` boundary but
    /// with no OS sandbox enforcing the leash — advisory only (a `top()` grant,
    /// or a host without Landlock/Seatbelt).
    Advisory,
    /// A remote (HTTP) server — no local process to confine.
    Remote,
}

/// The network-egress posture of a connected server, for the `/mcp` table
/// (#1243 Leg 4). Orthogonal to [`Confinement`] (the fs/exec sandbox): a server
/// can be fs-confined yet net-advisory, or net-gated with an advisory fs jail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NetGate {
    /// Egress routed through the loopback proxy or constrained to an approved,
    /// DNS-pinned private origin against an `n`-host allow-list.
    Gated(usize),
    /// No egress proxy or pinned-origin fence — outbound network is advisory.
    Advisory,
}

impl NetGate {
    pub(crate) fn from_posture(posture: newt_mcp_client::NetPosture) -> Self {
        match posture {
            newt_mcp_client::NetPosture::Gated(n) => Self::Gated(n),
            newt_mcp_client::NetPosture::Advisory => Self::Advisory,
        }
    }

    /// The `/mcp` suffix — always shown (net is a real axis for every server,
    /// remote or local).
    pub(crate) fn note(&self) -> String {
        match self {
            Self::Gated(n) => format!(" · net: gated ({n} host{})", if *n == 1 { "" } else { "s" }),
            Self::Advisory => " · net: advisory".to_string(),
        }
    }
}

impl Confinement {
    /// Map a connection's achieved [`newt_mcp_client::SandboxKind`] into the
    /// posture shown in `/mcp`. `None` = remote (no local process).
    pub(crate) fn from_sandbox(kind: Option<newt_mcp_client::SandboxKind>) -> Self {
        match kind {
            None => Self::Remote,
            Some(newt_mcp_client::SandboxKind::None) => Self::Advisory,
            Some(k) => Self::Confined(format!("{k:?}")),
        }
    }

    /// The suffix shown after the tool count in the `/mcp` table — empty for a
    /// remote server (its confinement is the server's own concern).
    pub(crate) fn note(&self) -> String {
        match self {
            Self::Confined(kind) => format!(" — confined: {kind}"),
            Self::Advisory => " — advisory (no OS sandbox)".to_string(),
            Self::Remote => String::new(),
        }
    }
}

/// The session's connected MCP servers.
#[derive(Clone)]
struct HttpReconnectState {
    entry: McpServerEntry,
    caveats: newt_core::caveats::Caveats,
    bearer: Option<String>,
    insecure_authorization_allowed: bool,
}

struct ReconnectableServer {
    live: ClientConnectedServer,
    http: Option<HttpReconnectState>,
}

pub(crate) struct Mcp {
    /// (server name, launch outcome) for every DISCOVERED entry — the `/mcp`
    /// status table (#1149). Includes disabled + skipped servers.
    pub(crate) statuses: Vec<(String, McpStatus)>,
    servers: Vec<ReconnectableServer>,
    /// Session-scoped mute set (`/mcp off <name>`). Muted servers stay
    /// *connected* (so `/mcp on` is instant) but their tools leave the
    /// advertised catalog and `handles`/`call` refuse them. Distinct from
    /// config `enabled = false` / [`Self::drop_server`] (`/mcp disable`),
    /// which is durable and tears the connection down.
    session_muted: std::collections::BTreeSet<String>,
    /// When `true`, hyphens in server names are replaced with underscores in
    /// advertised tool names and routing lookups.  Matches the behaviour of
    /// API proxies that normalise tool-name characters.  Controlled by
    /// `[tui].sanitize_mcp_server_names` in the newt config (default: `true`).
    sanitize_server_names: bool,
}

/// Apply or skip the hyphen→underscore normalisation for a server name.
fn server_prefix(name: &str, sanitize: bool) -> String {
    newt_core::mcp::runtime_server_prefix(name, sanitize)
}

/// Best-effort `(scheme, host)` from a URL — the canonical implementation
/// lives in `newt_mcp_client` (shared with `newt mcp probe`); this delegates
/// so the TUI's Bearer/egress gates can never diverge from it.
fn parse_scheme_host(url: Option<&str>) -> (String, String) {
    newt_mcp_client::parse_scheme_host(url)
}

/// A loopback host — the dev exception that needs no https and emits no warning.
/// Whether an HTTP MCP server at `host` may be dialed under the session net
/// scope (#1156). Empty host (no URL) and loopback are always allowed (dev /
/// no-egress); any other host must be permitted by the net allow-list.
fn http_egress_permitted(net: &newt_core::caveats::Scope<String>, host: &str) -> bool {
    host.is_empty() || newt_mcp_client::net_scope_permits_http_host(net, host)
}

fn host_is_loopback(host: &str) -> bool {
    // Canonical (IP-property, not string-prefix) check — `127.0.0.1.evil.com`
    // must never count as loopback for token injection or the egress gate.
    newt_mcp_client::host_is_loopback(host)
}

/// Whether an OAuth Bearer may be sent to `url` under the secure-by-default
/// transport policy (`docs/decisions/mcp_transport_security.md`): always over
/// `https` or to loopback; over a non-loopback `http://` host only when that host
/// is in `allow_insecure_hosts`. Unparseable/missing URL ⇒ withhold (fail safe).
fn bearer_allowed_for_url(url: Option<&str>, allow_insecure_hosts: &[String]) -> bool {
    let (scheme, host) = parse_scheme_host(url);
    if scheme == "https" || host_is_loopback(&host) {
        return true;
    }
    !host.is_empty()
        && allow_insecure_hosts
            .iter()
            .any(|h| newt_mcp_client::http_host_grant_matches(h, &host))
}

pub(crate) fn has_configured_authorization_header(entry: &McpServerEntry) -> bool {
    entry
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
}

pub(crate) fn has_plaintext_authorization_header(entry: &McpServerEntry) -> bool {
    entry.headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("authorization") && !authorization_is_reference(value)
    })
}

/// Configured Authorization credentials must remain references at rest. A raw
/// bearer string in TOML/borrowed JSON is easy to leak through audit, import,
/// or repository history; the auto-loaded OAuth token is injected only after
/// this check and is therefore unaffected.
fn authorization_is_reference(value: &newt_core::mcp::SecretValue) -> bool {
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

fn is_http_unauthorized(error: &anyhow::Error) -> bool {
    is_http_status(error, 401)
}

fn is_http_status(error: &anyhow::Error, expected: u16) -> bool {
    newt_mcp_client::http_error_status(error) == Some(expected)
}

async fn reconnect_http(
    state: &HttpReconnectState,
    bearer: Option<&str>,
) -> anyhow::Result<ClientConnectedServer> {
    let admitted = newt_core::mcp::admit(&state.entry)
        .map_err(|denied| anyhow::anyhow!("MCP reconnect was no longer admitted: {denied}"))?;
    connect_http_with_runtime_bearer(
        &admitted,
        &state.caveats,
        bearer,
        state.insecure_authorization_allowed,
    )
    .await
}

struct HttpConnectOutcome<T> {
    value: T,
    bearer: Option<String>,
}

async fn retry_http_unauthorized_once<T, Refresh, RefreshFuture, Reconnect, ReconnectFuture>(
    initial: anyhow::Result<T>,
    runtime_bearer: Option<String>,
    refresh: Refresh,
    reconnect: Reconnect,
) -> anyhow::Result<HttpConnectOutcome<T>>
where
    Refresh: FnOnce() -> RefreshFuture,
    RefreshFuture: std::future::Future<Output = Option<String>>,
    Reconnect: FnOnce(String) -> ReconnectFuture,
    ReconnectFuture: std::future::Future<Output = anyhow::Result<T>>,
{
    if runtime_bearer.is_some() && initial.as_ref().is_err_and(is_http_unauthorized) {
        if let Some(token) = refresh().await {
            let value = reconnect(token.clone()).await?;
            return Ok(HttpConnectOutcome {
                value,
                bearer: Some(token),
            });
        }
    }
    Ok(HttpConnectOutcome {
        value: initial?,
        bearer: runtime_bearer,
    })
}

/// Filter the optional runtime Bearer through the transport policy and report
/// whether this host received an explicit insecure-credential opt-in.
fn apply_transport_security(
    entry: &McpServerEntry,
    token: Option<String>,
    allow_insecure_hosts: &[String],
) -> (Option<String>, bool) {
    let (scheme, host) = parse_scheme_host(entry.url.as_deref());
    let secure = scheme == "https" || host_is_loopback(&host);
    let allowed = bearer_allowed_for_url(entry.url.as_deref(), allow_insecure_hosts);
    if !secure {
        // Policy: warn on every unencrypted (non-https, non-loopback) connection.
        match &token {
            Some(_) if allowed => tracing::warn!(
                "MCP server `{}`: UNENCRYPTED connection to `{}` (no TLS) — sending the \
                 OAuth Bearer anyway ([tui].mcp_allow_insecure_hosts opt-in)",
                entry.name,
                host
            ),
            Some(_) => tracing::warn!(
                "MCP server `{}`: UNENCRYPTED connection to `{}` (no TLS) — WITHHOLDING the \
                 OAuth Bearer token. Use https, or add `{}` to [tui].mcp_allow_insecure_hosts \
                 to override.",
                entry.name,
                host,
                host
            ),
            None => tracing::warn!(
                "MCP server `{}`: UNENCRYPTED connection to `{}` (no TLS).",
                entry.name,
                host
            ),
        }
    }
    let insecure_authorization_allowed = !secure && allowed;
    (token.filter(|_| allowed), insecure_authorization_allowed)
}

impl Mcp {
    /// Remove a live server by name (`/mcp disable`, #1149): its tools leave
    /// the surface immediately; config persistence is the caller's job.
    /// Also clears any session mute for that name (the connection is gone).
    pub(crate) fn drop_server(&mut self, name: &str) {
        self.servers.retain(|s| s.live.name != name);
        self.session_muted.remove(name);
    }

    /// Whether `name` is currently session-muted (`/mcp off`).
    #[must_use]
    /// The `/mcp` status view, one line per server.
    ///
    /// # Why this is a method rather than forty lines in `chat.rs`
    ///
    /// It was inline in the `/mcp` arm, which meant the only way to see what
    /// it renders was to run a session with servers configured. The Session
    /// cockpit needs the same lines for its MCP section (#2009 PR10a2), and a
    /// second rendering of the same statuses is how a panel and its command
    /// come to disagree about whether a server is muted.
    ///
    /// Pure over `self`, so the unit tier checks every status arm — including
    /// the auth hint — with no connection and no config.
    pub(crate) fn status_lines(&self) -> Vec<String> {
        if self.statuses.is_empty() {
            return vec![
                "no MCP servers configured — add [[mcp_servers]] to ~/.newt/config.toml"
                    .to_string(),
            ];
        }
        let mut out = vec!["MCP servers:".to_string()];
        for (n, st) in &self.statuses {
            out.push(match st {
                McpStatus::Connected {
                    tools,
                    confinement,
                    net,
                } => {
                    if self.is_muted(n) {
                        format!(
                            "  {n}  ⏸ muted this session ({tools} tools still connected — /mcp on {n}){}{}",
                            confinement.note(),
                            net.note()
                        )
                    } else {
                        format!(
                            "  {n}  ✓ connected ({tools} tools){}{}",
                            confinement.note(),
                            net.note()
                        )
                    }
                }
                McpStatus::Skipped(r) => {
                    // A 401 is the one skip an operator can act on, so it says
                    // how — the rest report what happened and stop.
                    let hint = if r.contains("401") || r.to_lowercase().contains("auth") {
                        format!(" — `newt auth {n}` to re-authenticate")
                    } else {
                        String::new()
                    };
                    format!("  {n}  ✗ skipped: {r}{hint}")
                }
                McpStatus::Disabled => {
                    format!("  {n}  ⏸ disabled in config (/mcp enable {n})")
                }
            });
        }
        out
    }

    pub(crate) fn is_muted(&self, name: &str) -> bool {
        self.session_muted.contains(name)
    }

    /// Whether a connected server is advertising tools this turn (connected
    /// and not session-muted).
    fn is_advertising(&self, server: &ReconnectableServer) -> bool {
        !self.session_muted.contains(&server.live.name)
    }

    /// Session-mute a connected server (`/mcp off <name>`). Keeps the
    /// connection alive so `/mcp on` is instant. Returns `false` when no
    /// connected server matches `name`.
    pub(crate) fn mute(&mut self, name: &str) -> bool {
        let connected = self.servers.iter().any(|s| s.live.name == name)
            || self
                .statuses
                .iter()
                .any(|(n, st)| n == name && matches!(st, McpStatus::Connected { .. }));
        if !connected {
            return false;
        }
        self.session_muted.insert(name.to_owned());
        true
    }

    /// Clear a session mute (`/mcp on <name>`). Returns `false` when no
    /// connected server matches `name` (config-disabled / skipped servers
    /// cannot be unmuted — use `/mcp enable` + relaunch, or #1148).
    pub(crate) fn unmute(&mut self, name: &str) -> bool {
        let connected = self.servers.iter().any(|s| s.live.name == name)
            || self
                .statuses
                .iter()
                .any(|(n, st)| n == name && matches!(st, McpStatus::Connected { .. }));
        if !connected {
            return false;
        }
        self.session_muted.remove(name);
        true
    }

    /// Mute every currently connected server (`/mcp off`). Returns the names
    /// that were muted.
    pub(crate) fn mute_all(&mut self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> =
            self.servers.iter().map(|s| s.live.name.clone()).collect();
        for (n, st) in &self.statuses {
            if matches!(st, McpStatus::Connected { .. }) {
                names.insert(n.clone());
            }
        }
        let names: Vec<String> = names.into_iter().collect();
        for n in &names {
            self.session_muted.insert(n.clone());
        }
        names
    }

    /// Unmute every session-muted server (`/mcp on`). Returns the names that
    /// were unmuted.
    pub(crate) fn unmute_all(&mut self) -> Vec<String> {
        let names: Vec<String> = self.session_muted.iter().cloned().collect();
        self.session_muted.clear();
        names
    }

    /// An empty set — connects to nothing. Used by tests (the live session
    /// always builds via [`Self::connect`]).
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            statuses: Vec::new(),
            servers: Vec::new(),
            session_muted: std::collections::BTreeSet::new(),
            sanitize_server_names: true,
        }
    }

    /// Discover (newt config + Claude Code config) and connect to every **stdio**
    /// MCP server. A server that fails to spawn/initialize is logged and skipped
    /// — one bad server never blocks the session or the others.
    pub(crate) async fn connect(
        workspace: &str,
        cfg_servers: &[McpServerEntry],
        sanitize_server_names: bool,
        allow_insecure_hosts: &[String],
        // The session's Caveats leash. #1156: an HTTP MCP server's egress is
        // gated by its `net` axis (same allow-list as a shell `curl`), so a
        // confined session can't reach an un-granted host via a rogue MCP config.
        // #1243 Leg 3: a spawned stdio server is confined to this whole leash.
        caveats: &newt_core::caveats::Caveats,
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
        let mut statuses: Vec<(String, McpStatus)> = Vec::new();
        for entry in &entries {
            if !entry.enabled {
                statuses.push((entry.name.clone(), McpStatus::Disabled));
                continue;
            }
            // Dispatch on transport. The legacy SSE-only transport (a separate
            // GET event-stream + POST endpoint) is not implemented; modern
            // servers use streamable-HTTP (`type: "http"`).
            let result = match entry.transport {
                // step-1.1: admission gate before spawn. An untrusted repo
                // overlay (`.mcp.json` / project config) with no approval is
                // refused here, not connected. (The interactive approval path
                // is a follow-up; until then untrusted fails closed.)
                TransportKind::Stdio => match newt_core::mcp::admit(entry) {
                    Ok(admitted) => connect_stdio(&admitted, caveats)
                        .await
                        .map(|live| ReconnectableServer { live, http: None }),
                    Err(denied) => {
                        tracing::warn!("MCP server `{}` not admitted: {denied}", entry.name);
                        statuses.push((entry.name.clone(), McpStatus::Skipped(denied.to_string())));
                        continue;
                    }
                },
                TransportKind::Http => {
                    // Admission is the first operation in the HTTP branch.
                    // Token loading may refresh over the network, so even that
                    // convenience path must be unreachable without the same
                    // trust witness required by the transport constructor.
                    let admitted = match newt_core::mcp::admit(entry) {
                        Ok(admitted) => admitted,
                        Err(denied) => {
                            tracing::warn!("MCP server `{}` not admitted: {denied}", entry.name);
                            statuses
                                .push((entry.name.clone(), McpStatus::Skipped(denied.to_string())));
                            continue;
                        }
                    };
                    // #1156: net-gate egress. A loopback host is the dev
                    // exception (never leaves the box); any other host must be
                    // permitted by the session net scope or the server is
                    // skipped (shown in /mcp), never silently dialed.
                    let (_scheme, host) = parse_scheme_host(entry.url.as_deref());
                    if !http_egress_permitted(&caveats.net, &host) {
                        tracing::warn!(
                            "MCP server `{}`: egress to {host} is outside the session net \
                             allow-list — skipped (grant it in [tui.permissions] net)",
                            entry.name
                        );
                        statuses.push((
                            entry.name.clone(),
                            McpStatus::Skipped(format!("net not granted: {host}")),
                        ));
                        continue;
                    }
                    if has_plaintext_authorization_header(entry) {
                        let reason = "plaintext Authorization credential in MCP config; replace it with an environment/file reference";
                        tracing::warn!("MCP server `{}`: {reason} — skipped", entry.name);
                        statuses.push((entry.name.clone(), McpStatus::Skipped(reason.to_string())));
                        continue;
                    }
                    // Load the stored hermes OAuth token only when the operator
                    // hasn't already configured an explicit Authorization header.
                    let already_authed = has_configured_authorization_header(entry);
                    let oauth_policy = crate::mcp_token::OAuthHopPolicy::new(&caveats.net);
                    let token = if already_authed {
                        None
                    } else {
                        match entry.url.as_deref() {
                            Some(url) => {
                                crate::mcp_token::load_bearer_token(&entry.name, url, &oauth_policy)
                                    .await
                            }
                            None => None,
                        }
                    };
                    // Secure-by-default transport policy: WARN on any non-loopback
                    // unencrypted connection, and only inject the OAuth Bearer over
                    // https / loopback / an explicitly allow-listed host
                    // (docs/decisions/mcp_transport_security.md).
                    let (token, insecure_authorization_allowed) =
                        apply_transport_security(entry, token, allow_insecure_hosts);
                    let rejected_bearer = token.clone();
                    let reconnect_entry = entry.clone();
                    let reconnect_caveats = caveats.clone();
                    // #1243 Leg 4: route the HTTP client through the session's
                    // egress proxy so per-call traffic + redirects are net-gated,
                    // not just the connect-time host (#1156).
                    let initial = connect_http_with_runtime_bearer(
                        &admitted,
                        caveats,
                        token.as_deref(),
                        insecure_authorization_allowed,
                    )
                    .await;
                    // A token can be revoked or expire earlier than its local
                    // timestamp. Refresh under the credential transaction and
                    // retry the MCP handshake exactly once on a typed 401.
                    retry_http_unauthorized_once(
                        initial,
                        token,
                        || async {
                            let url = entry.url.as_deref()?;
                            let rejected = rejected_bearer.as_deref()?;
                            crate::mcp_token::refresh_bearer_token(
                                &entry.name,
                                url,
                                rejected,
                                &oauth_policy,
                            )
                            .await
                        },
                        |refreshed| async move {
                            let (retry_token, retry_insecure_allowed) = apply_transport_security(
                                entry,
                                Some(refreshed),
                                allow_insecure_hosts,
                            );
                            connect_http_with_runtime_bearer(
                                &admitted,
                                caveats,
                                retry_token.as_deref(),
                                retry_insecure_allowed,
                            )
                            .await
                        },
                    )
                    .await
                    .map(|outcome| ReconnectableServer {
                        live: outcome.value,
                        http: Some(HttpReconnectState {
                            entry: reconnect_entry,
                            caveats: reconnect_caveats,
                            bearer: outcome.bearer,
                            insecure_authorization_allowed,
                        }),
                    })
                }
                TransportKind::Sse => {
                    tracing::warn!(
                        "MCP server `{}`: legacy SSE transport is not supported \
                         (use streamable-HTTP, `type = \"http\"`) — skipped",
                        entry.name
                    );
                    statuses.push((
                        entry.name.clone(),
                        McpStatus::Skipped("legacy SSE transport (use type = \"http\")".into()),
                    ));
                    continue;
                }
            };
            match result {
                Ok(connected) => {
                    let prefix = server_prefix(&connected.live.name, sanitize_server_names);
                    if !connected_prefixes.insert(prefix.clone()) {
                        let reason = format!("emitted namespace `{prefix}` is already connected");
                        tracing::warn!("MCP server `{}` skipped: {reason}", connected.live.name);
                        statuses.push((entry.name.clone(), McpStatus::Skipped(reason)));
                        continue;
                    }
                    statuses.push((
                        entry.name.clone(),
                        McpStatus::Connected {
                            tools: connected.live.tools.len(),
                            confinement: Confinement::from_sandbox(connected.live.sandbox_kind),
                            net: NetGate::from_posture(connected.live.net_posture),
                        },
                    ));
                    servers.push(connected);
                }
                Err(e) => {
                    tracing::warn!("MCP server `{}` skipped: {e:#}", entry.name);
                    statuses.push((entry.name.clone(), McpStatus::Skipped(format!("{e:#}"))));
                }
            }
        }
        Self {
            statuses,
            servers,
            session_muted: std::collections::BTreeSet::new(),
            sanitize_server_names,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// `(server_name, tool_count)` for each connected server — for the ready line.
    pub(crate) fn summary(&self) -> Vec<(String, usize)> {
        self.servers
            .iter()
            .map(|s| (s.live.name.clone(), s.live.tools.len()))
            .collect()
    }

    /// OpenAI-style function tool definitions for every remote tool, with names
    /// namespaced `server__tool` so two servers cannot collide.
    ///
    /// Server names are sanitized (hyphens → underscores) before advertising
    /// because some API proxies (e.g. NVIDIA inference → Anthropic backend)
    /// normalise hyphens in tool names to underscores.  Advertising the
    /// sanitized form ensures the model's tool calls round-trip back unchanged.
    pub(crate) fn tool_defs(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for server in &self.servers {
            if !self.is_advertising(server) {
                continue;
            }
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

    /// Whether `name` is a namespaced tool belonging to a connected server.
    ///
    /// Matches the sanitized form (hyphens → underscores in the server prefix)
    /// so that a tool advertised as `acme_server__X` routes to the server
    /// stored as `acme-server`. Session-muted servers do not handle calls.
    pub(crate) fn handles(&self, name: &str) -> bool {
        match split_namespaced(name) {
            Some((server, _)) => self.servers.iter().any(|s| {
                self.is_advertising(s)
                    && server_prefix(&s.live.name, self.sanitize_server_names) == server
            }),
            None => false,
        }
    }

    /// Route a `server__tool` call to its server and render the result as the
    /// string the agent loop feeds back as the tool message.
    pub(crate) async fn call(&mut self, name: &str, args: &Value) -> String {
        let Some((server_name, tool)) = split_namespaced(name) else {
            return format!("error: `{name}` is not a namespaced MCP tool");
        };
        // Check mute before taking a mutable borrow of `servers`.
        if let Some(muted) = self
            .session_muted
            .iter()
            .find(|n| server_prefix(n, self.sanitize_server_names) == server_name)
        {
            return format!(
                "error: MCP server `{muted}` is muted this session — `/mcp on {muted}` to restore its tools"
            );
        }
        let Some(server) = self
            .servers
            .iter_mut()
            .find(|s| server_prefix(&s.live.name, self.sanitize_server_names) == server_name)
        else {
            return format!("error: no connected MCP server `{server_name}`");
        };
        let had_session = server.live.conn.has_session();
        match server.live.conn.call_tool(tool, args.clone()).await {
            // Scoped FR-14 (#1042): the result body is external data from the
            // connected server, not a newt-generated message — wrap it so the
            // model treats it as information, not instructions. `e` below is
            // OUR OWN connection-error text, not external content, so it is
            // NOT wrapped.
            Ok(result) => newt_core::wrap_untrusted(name, &format_result(&result)),
            Err(error) => {
                let Some(state) = server.http.clone() else {
                    return format!("error: {error}");
                };
                let original_error = error.to_string();
                let configured_authorization = has_configured_authorization_header(&state.entry);
                let refresh_state = state.clone();
                let reconnect_state = state.clone();
                let replay_tool = tool.to_string();
                let replay_args = args.clone();
                let recovered = newt_mcp_client::recover_http_call_after_error(
                    error,
                    had_session,
                    state.bearer.clone(),
                    configured_authorization,
                    move |rejected| {
                        let refresh_state = refresh_state.clone();
                        async move {
                            let url = refresh_state.entry.url.as_deref()?;
                            let policy =
                                crate::mcp_token::OAuthHopPolicy::new(&refresh_state.caveats.net);
                            crate::mcp_token::refresh_bearer_token(
                                &refresh_state.entry.name,
                                url,
                                &rejected,
                                &policy,
                            )
                            .await
                        }
                    },
                    move |bearer| {
                        let reconnect_state = reconnect_state.clone();
                        async move { reconnect_http(&reconnect_state, bearer.as_deref()).await }
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
                let outcome = match recovered {
                    Ok(Some(recovered)) => recovered,
                    Ok(None) => return format!("error: {original_error}"),
                    Err(reconnect_error) => {
                        return format!(
                            "error: {original_error}; MCP recovery failed: {reconnect_error}"
                        )
                    }
                };
                // `connect_http_with_runtime_bearer` completed initialize and
                // tools/list and the bounded state machine replayed the call.
                // Swap the whole live server so later calls use the recovered
                // session, catalog, and bearer too.
                server.live = outcome.connection;
                if let Some(http) = server.http.as_mut() {
                    http.bearer = outcome.bearer;
                }
                match outcome.result {
                    Ok(result) => newt_core::wrap_untrusted(name, &format_result(&result)),
                    Err(retry_error) => {
                        format!("error: {original_error}; MCP recovery failed: {retry_error}")
                    }
                }
            }
        }
    }
}

/// Bridge into the agentic loop (Step 9.7): `newt_core::agentic` cannot name
/// this type without a `newt-core` ← `newt-mcp-client` dependency cycle, so
/// the loop takes the minimal [`McpTools`] seam and the TUI forwards to the
/// inherent methods above.
#[async_trait::async_trait]
impl newt_core::agentic::McpTools for Mcp {
    fn handles(&self, name: &str) -> bool {
        Self::handles(self, name)
    }
    fn tool_defs(&self) -> Vec<Value> {
        Self::tool_defs(self)
    }
    async fn call(&mut self, leased: &newt_core::agentic::LeasedMcpCall<'_>) -> String {
        // The witness carries the leash-approved tool name + args; forward to the
        // inherent implementation.
        Self::call(self, leased.tool(), leased.args()).await
    }
}

/// Flatten an MCP `tools/call` result (`{ content: [{type,text}], isError? }`)
/// into agent-facing text. Falls back to raw JSON if there is no text content.
fn format_result(result: &Value) -> String {
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
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_handles_nothing_and_has_no_defs() {
        let mcp = Mcp::empty();
        assert!(mcp.is_empty());
        assert!(!mcp.handles("git__status"));
        assert!(mcp.tool_defs().is_empty());
    }

    #[test]
    fn session_mute_round_trip_on_connected_status() {
        // Status-only Connected entry (no live transport) is enough to exercise
        // the mute set — the advertise path filters on `session_muted`.
        let mut mcp = Mcp::empty();
        mcp.statuses.push((
            "github".into(),
            McpStatus::Connected {
                tools: 4,
                confinement: Confinement::Remote,
                net: NetGate::Advisory,
            },
        ));
        assert!(!mcp.is_muted("github"));
        assert!(mcp.mute("github"));
        assert!(mcp.is_muted("github"));
        assert!(mcp.unmute("github"));
        assert!(!mcp.is_muted("github"));
    }

    #[test]
    fn mute_unknown_or_disabled_server_fails() {
        let mut mcp = Mcp::empty();
        mcp.statuses.push(("dead".into(), McpStatus::Disabled));
        assert!(!mcp.mute("dead"));
        assert!(!mcp.mute("missing"));
        assert!(!mcp.unmute("missing"));
    }

    #[test]
    fn mute_all_and_unmute_all() {
        let mut mcp = Mcp::empty();
        mcp.statuses.push((
            "a".into(),
            McpStatus::Connected {
                tools: 1,
                confinement: Confinement::Remote,
                net: NetGate::Advisory,
            },
        ));
        mcp.statuses.push((
            "b".into(),
            McpStatus::Connected {
                tools: 2,
                confinement: Confinement::Remote,
                net: NetGate::Advisory,
            },
        ));
        let muted = mcp.mute_all();
        assert_eq!(muted, vec!["a".to_string(), "b".to_string()]);
        assert!(mcp.is_muted("a"));
        assert!(mcp.is_muted("b"));
        let unmuted = mcp.unmute_all();
        assert_eq!(unmuted, vec!["a".to_string(), "b".to_string()]);
        assert!(!mcp.is_muted("a"));
        assert!(!mcp.is_muted("b"));
    }

    #[test]
    fn drop_server_clears_session_mute() {
        let mut mcp = Mcp::empty();
        mcp.statuses.push((
            "x".into(),
            McpStatus::Connected {
                tools: 1,
                confinement: Confinement::Remote,
                net: NetGate::Advisory,
            },
        ));
        assert!(mcp.mute("x"));
        mcp.drop_server("x");
        assert!(!mcp.is_muted("x"));
    }

    #[test]
    fn confinement_maps_sandbox_kind_to_posture() {
        use newt_mcp_client::SandboxKind;
        // No local process (HTTP) → Remote.
        assert_eq!(Confinement::from_sandbox(None), Confinement::Remote);
        // Spawned but nothing kernel-confined → Advisory.
        assert_eq!(
            Confinement::from_sandbox(Some(SandboxKind::None)),
            Confinement::Advisory
        );
        // A real OS sandbox → Confined, carrying the achieved kind's name.
        assert_eq!(
            Confinement::from_sandbox(Some(SandboxKind::Landlock)),
            Confinement::Confined("Landlock".to_string())
        );
    }

    #[test]
    fn confinement_note_renders_each_posture() {
        assert_eq!(Confinement::Remote.note(), "");
        assert_eq!(Confinement::Advisory.note(), " — advisory (no OS sandbox)");
        assert_eq!(
            Confinement::Confined("Landlock".to_string()).note(),
            " — confined: Landlock"
        );
    }

    #[test]
    fn net_gate_maps_posture_and_renders_note() {
        use newt_mcp_client::NetPosture;
        // Mapping from the client posture.
        assert_eq!(
            NetGate::from_posture(NetPosture::Advisory),
            NetGate::Advisory
        );
        assert_eq!(
            NetGate::from_posture(NetPosture::Gated(3)),
            NetGate::Gated(3)
        );
        // Rendered `/mcp` suffixes — singular/plural, always shown.
        assert_eq!(NetGate::Advisory.note(), " · net: advisory");
        assert_eq!(NetGate::Gated(1).note(), " · net: gated (1 host)");
        assert_eq!(NetGate::Gated(2).note(), " · net: gated (2 hosts)");
    }

    // ── transport security: the OAuth Bearer must never go over plaintext ──

    #[test]
    fn unauthorized_retry_detection_requires_a_typed_401() {
        let unauthorized = anyhow::Error::new(newt_mcp_client::HttpStatusError::new(
            401,
            "Unauthorized",
            "",
        ))
        .context("initial MCP handshake failed");
        assert!(is_http_unauthorized(&unauthorized));

        let forbidden =
            anyhow::Error::new(newt_mcp_client::HttpStatusError::new(403, "Forbidden", ""));
        assert!(!is_http_unauthorized(&forbidden));
        let missing_session =
            anyhow::Error::new(newt_mcp_client::HttpStatusError::new(404, "Not Found", ""));
        assert!(is_http_status(&missing_session, 404));
        assert!(!is_http_status(&missing_session, 401));
        assert!(!is_http_unauthorized(&anyhow::anyhow!(
            "server text happened to contain 401 Unauthorized"
        )));
    }

    #[tokio::test]
    async fn unauthorized_refresh_and_reconnect_are_attempted_exactly_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let refreshes = Arc::new(AtomicUsize::new(0));
        let reconnects = Arc::new(AtomicUsize::new(0));
        let refresh_counter = Arc::clone(&refreshes);
        let reconnect_counter = Arc::clone(&reconnects);
        let initial: anyhow::Result<()> = Err(anyhow::Error::new(
            newt_mcp_client::HttpStatusError::new(401, "Unauthorized", ""),
        ));

        let result = retry_http_unauthorized_once(
            initial,
            Some("rejected-token".to_string()),
            move || async move {
                refresh_counter.fetch_add(1, Ordering::SeqCst);
                Some("refreshed-token".to_string())
            },
            move |token| async move {
                assert_eq!(token, "refreshed-token");
                reconnect_counter.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::Error::new(newt_mcp_client::HttpStatusError::new(
                    401,
                    "Unauthorized",
                    "",
                )))
            },
        )
        .await;

        assert!(result.is_err());
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(reconnects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn expired_session_then_unauthorized_replay_refreshes_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let refreshes = Arc::new(AtomicUsize::new(0));
        let reconnects = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let refresh_counter = Arc::clone(&refreshes);
        let reconnect_counter = Arc::clone(&reconnects);
        let call_counter = Arc::clone(&calls);
        let initial =
            anyhow::Error::new(newt_mcp_client::HttpStatusError::new(404, "Not Found", ""));

        let outcome = newt_mcp_client::recover_http_call_after_error(
            initial,
            true,
            Some("stale-token".to_string()),
            false,
            move |rejected| {
                let refresh_counter = Arc::clone(&refresh_counter);
                async move {
                    assert_eq!(rejected, "stale-token");
                    refresh_counter.fetch_add(1, Ordering::SeqCst);
                    Some("fresh-token".to_string())
                }
            },
            move |bearer| {
                let attempt = reconnect_counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        assert_eq!(bearer.as_deref(), Some("stale-token"));
                        Ok("stale-connection")
                    } else {
                        assert_eq!(bearer.as_deref(), Some("fresh-token"));
                        Ok("fresh-connection")
                    }
                }
            },
            move |connection| {
                let attempt = call_counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    let result = if attempt == 0 {
                        assert_eq!(connection, "stale-connection");
                        Err(anyhow::Error::new(newt_mcp_client::HttpStatusError::new(
                            401,
                            "Unauthorized",
                            "",
                        )))
                    } else {
                        assert_eq!(connection, "fresh-connection");
                        Ok("review-loaded")
                    };
                    (connection, result)
                }
            },
        )
        .await
        .unwrap()
        .expect("the bounded recovery sequence succeeds");

        assert_eq!(outcome.connection, "fresh-connection");
        assert_eq!(outcome.result.unwrap(), "review-loaded");
        assert_eq!(outcome.bearer.as_deref(), Some("fresh-token"));
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(reconnects.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn expired_session_and_bearer_refresh_stop_after_final_unauthorized_replay() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let refreshes = Arc::new(AtomicUsize::new(0));
        let reconnects = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let refresh_counter = Arc::clone(&refreshes);
        let reconnect_counter = Arc::clone(&reconnects);
        let call_counter = Arc::clone(&calls);
        let initial =
            anyhow::Error::new(newt_mcp_client::HttpStatusError::new(404, "Not Found", ""));

        let result =
            newt_mcp_client::recover_http_call_after_error(
                initial,
                true,
                Some("stale-token".to_string()),
                false,
                move |_rejected| {
                    refresh_counter.fetch_add(1, Ordering::SeqCst);
                    async { Some("fresh-token".to_string()) }
                },
                move |bearer| {
                    reconnect_counter.fetch_add(1, Ordering::SeqCst);
                    async move { Ok(bearer.expect("both reconnects carry a runtime bearer")) }
                },
                move |connection| {
                    call_counter.fetch_add(1, Ordering::SeqCst);
                    async move {
                        (
                            connection,
                            Err::<(), _>(anyhow::Error::new(
                                newt_mcp_client::HttpStatusError::new(401, "Unauthorized", ""),
                            )),
                        )
                    }
                },
            )
            .await;

        let outcome = result
            .unwrap()
            .expect("the final failed replay still returns the recovered connection");
        assert!(outcome.result.is_err());
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(reconnects.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn configured_authorization_recovers_after_session_reset_and_replay_401() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let reconnects = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let reconnect_counter = Arc::clone(&reconnects);
        let call_counter = Arc::clone(&calls);
        let initial =
            anyhow::Error::new(newt_mcp_client::HttpStatusError::new(404, "Not Found", ""));
        let outcome = newt_mcp_client::recover_http_call_after_error(
            initial,
            true,
            None,
            true,
            |_| async { panic!("configured credentials are re-resolved, not OAuth-refreshed") },
            move |bearer| {
                let attempt = reconnect_counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    assert!(bearer.is_none());
                    Ok(attempt)
                }
            },
            move |connection| {
                let attempt = call_counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    let result = if attempt == 0 {
                        Err(anyhow::Error::new(newt_mcp_client::HttpStatusError::new(
                            401,
                            "Unauthorized",
                            "",
                        )))
                    } else {
                        Ok("accepted")
                    };
                    (connection, result)
                }
            },
        )
        .await
        .unwrap()
        .expect("configured Authorization recovery succeeds");

        assert_eq!(outcome.connection, 1);
        assert_eq!(outcome.result.unwrap(), "accepted");
        assert!(outcome.bearer.is_none());
        assert_eq!(reconnects.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    fn http_entry(url: &str) -> McpServerEntry {
        McpServerEntry {
            enabled: true,
            name: "MaaS".into(),
            transport: TransportKind::Http,
            command: None,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: Some(url.into()),
            headers: std::collections::BTreeMap::new(),
            request_timeout_secs: None,
            trust: newt_core::mcp::McpTrust::Trusted,
        }
    }

    #[test]
    fn parse_scheme_host_handles_common_shapes() {
        assert_eq!(
            parse_scheme_host(Some("https://a.b/c")),
            ("https".into(), "a.b".into())
        );
        assert_eq!(
            parse_scheme_host(Some("http://127.0.0.1:8080/x")),
            ("http".into(), "127.0.0.1".into())
        );
        assert_eq!(
            parse_scheme_host(Some("http://u@Host:9/x")),
            ("http".into(), "host".into())
        );
        assert_eq!(
            parse_scheme_host(Some("http://[::1]:7/x")),
            ("http".into(), "::1".into())
        );
        assert_eq!(parse_scheme_host(None), (String::new(), String::new()));
    }

    #[test]
    fn bearer_allowed_only_over_https_loopback_or_allowlist() {
        let none: &[String] = &[];
        assert!(bearer_allowed_for_url(
            Some("https://api.maas.com/mcp"),
            none
        ));
        assert!(bearer_allowed_for_url(Some("http://localhost:9/mcp"), none));
        assert!(bearer_allowed_for_url(Some("http://127.0.0.1:9/mcp"), none));
        assert!(bearer_allowed_for_url(Some("http://[::1]:9/mcp"), none));
        // plain http to a real host: NOT allowed by default (the blocker)
        assert!(!bearer_allowed_for_url(
            Some("http://api.maas.com/mcp"),
            none
        ));
        assert!(!bearer_allowed_for_url(None, none));
        // …unless the host is explicitly allow-listed (case-insensitive)
        let allow = vec!["api.maas.com".to_string()];
        assert!(bearer_allowed_for_url(
            Some("http://API.MaaS.com/mcp"),
            &allow
        ));
    }

    #[test]
    fn http_egress_net_gate() {
        use newt_core::caveats::Scope;
        // #1156: loopback + empty always allowed; a remote host needs a grant.
        let none: Scope<String> = Scope::only::<Vec<String>>(vec![]);
        assert!(http_egress_permitted(&none, ""));
        assert!(http_egress_permitted(&none, "127.0.0.1"));
        assert!(http_egress_permitted(&none, "localhost"));
        assert!(!http_egress_permitted(&none, "mcp.example.com"));
        let granted = Scope::only(["mcp.example.com".to_string()]);
        assert!(http_egress_permitted(&granted, "mcp.example.com"));
        assert!(!http_egress_permitted(&granted, "evil.example.com"));
        // Scope::All (unconfined / --full-access) permits any host.
        assert!(http_egress_permitted(&Scope::All, "anything.example.com"));
    }

    #[test]
    fn authorization_header_requires_a_secret_reference_at_rest() {
        use newt_core::mcp::SecretValue;

        assert!(!authorization_is_reference(&SecretValue::literal(
            "Bearer plaintext-secret"
        )));
        assert!(!authorization_is_reference(&SecretValue::literal("${}")));
        assert!(authorization_is_reference(&SecretValue::literal(
            "Bearer ${env:MCP_TOKEN}"
        )));
        assert!(authorization_is_reference(&SecretValue::literal(
            "${file:/run/secrets/mcp-token}"
        )));

        let mut entry = http_entry("https://mcp.example.test/mcp");
        entry.headers.insert(
            "aUtHoRiZaTiOn".into(),
            SecretValue::literal("Bearer ${env:MCP_TOKEN}"),
        );
        assert!(has_configured_authorization_header(&entry));
        assert!(!has_plaintext_authorization_header(&entry));
        entry.headers.insert(
            "Authorization".into(),
            SecretValue::literal("Bearer plaintext-secret"),
        );
        assert!(has_plaintext_authorization_header(&entry));
    }

    #[test]
    fn plaintext_authorization_is_invalid_not_a_configured_credential() {
        use newt_core::mcp::SecretValue;

        let mut entry = http_entry("https://mcp.example.test/mcp");
        entry.headers.insert(
            "Authorization".into(),
            SecretValue::literal("Bearer plaintext-secret"),
        );
        assert!(has_configured_authorization_header(&entry));
        assert!(has_plaintext_authorization_header(&entry));

        entry.headers.insert(
            "Authorization".into(),
            SecretValue::literal("Bearer ${env:MCP_TOKEN}"),
        );
        assert!(has_configured_authorization_header(&entry));
        assert!(!has_plaintext_authorization_header(&entry));
    }

    #[test]
    fn untrusted_http_entry_cannot_reach_the_credential_stage() {
        let mut entry = http_entry("https://127.0.0.1:9/mcp");
        entry.trust = newt_core::mcp::McpTrust::Untrusted;
        let credential_stage_reached = std::cell::Cell::new(false);

        // This mirrors the first operation in `Mcp::connect`'s HTTP branch:
        // only possession of the admission witness permits execution to move
        // on to token-file reads or refresh network requests.
        if newt_core::mcp::admit(&entry).is_ok() {
            credential_stage_reached.set(true);
        }

        assert!(!credential_stage_reached.get());
        assert_eq!(
            newt_core::mcp::admit(&entry).unwrap_err(),
            newt_core::mcp::AdmissionDenied::UntrustedNotApproved
        );
    }

    #[test]
    fn apply_transport_security_withholds_token_over_plain_http() {
        // the blocker: a stored Bearer must NOT be injected over plaintext http
        let e = http_entry("http://api.example.test/mcp");
        let (token, insecure) = apply_transport_security(&e, Some("SECRET".into()), &[]);
        assert_eq!(token, None);
        assert!(!insecure);
    }

    #[test]
    fn apply_transport_security_injects_over_https_and_allowlisted() {
        let https = http_entry("https://api.example.test/mcp");
        let (token, insecure) = apply_transport_security(&https, Some("SECRET".into()), &[]);
        assert_eq!(token.as_deref(), Some("SECRET"));
        assert!(!insecure);

        let allowed = http_entry("http://api.example.test/mcp");
        let (token, insecure) = apply_transport_security(
            &allowed,
            Some("SECRET".into()),
            &["api.example.test".to_string()],
        );
        assert_eq!(token.as_deref(), Some("SECRET"));
        assert!(insecure);

        let loopback = http_entry("http://127.0.0.1:9/mcp");
        let (token, insecure) = apply_transport_security(&loopback, Some("SECRET".into()), &[]);
        assert_eq!(token.as_deref(), Some("SECRET"));
        assert!(!insecure);
    }

    /// Some OpenAI-compatible API proxies normalise hyphens to underscores in
    /// tool names.  Verify the `server_prefix` helper obeys the toggle.
    #[test]
    fn server_prefix_toggle() {
        // sanitize=true: hyphens become underscores
        assert_eq!(server_prefix("acme-server", true), "acme_server");
        assert_eq!(server_prefix("multi-part-name", true), "multi_part_name");
        assert_eq!(server_prefix("plainserver", true), "plainserver");

        // sanitize=false: name is returned unchanged
        assert_eq!(server_prefix("acme-server", false), "acme-server");
        assert_eq!(server_prefix("multi-part-name", false), "multi-part-name");
        assert_eq!(server_prefix("plainserver", false), "plainserver");

        // Double-underscore separator is preserved after sanitization.
        let tool = format!("{}__probe_tool", server_prefix("acme-server", true));
        assert_eq!(tool, "acme_server__probe_tool");
    }

    #[test]
    fn format_result_joins_text_content() {
        let r =
            json!({ "content": [{"type":"text","text":"hello"},{"type":"text","text":"world"}] });
        assert_eq!(format_result(&r), "hello\nworld");
    }

    #[test]
    fn format_result_flags_errors_and_falls_back_to_json() {
        let err = json!({ "content": [{"type":"text","text":"boom"}], "isError": true });
        assert_eq!(format_result(&err), "tool error: boom");
        // No text content → raw JSON fallback (still informative).
        let weird = json!({ "structured": 1 });
        assert!(format_result(&weird).contains("structured"));
    }

    /// **Every status arm renders, checked without a connection.**
    ///
    /// These forty lines lived inline in `chat.rs`'s `/mcp` arm until #2009
    /// PR10a2, which meant the only way to see what they rendered was to run a
    /// session with servers configured — so the auth hint, the muted wording
    /// and the disabled row had never been asserted at all.
    #[test]
    fn status_lines_render_every_arm() {
        let mut mcp = Mcp::empty();
        mcp.statuses = vec![
            (
                "github".to_string(),
                McpStatus::Connected {
                    tools: 12,
                    confinement: Confinement::from_sandbox(None),
                    net: NetGate::from_posture(newt_mcp_client::NetPosture::Advisory),
                },
            ),
            (
                "stale".to_string(),
                McpStatus::Skipped("HTTP 401".to_string()),
            ),
            (
                "other".to_string(),
                McpStatus::Skipped("connect timeout".to_string()),
            ),
            ("off".to_string(), McpStatus::Disabled),
        ];

        let lines = mcp.status_lines();
        assert_eq!(lines[0], "MCP servers:");
        assert!(lines[1].contains("github") && lines[1].contains("✓ connected (12 tools)"));

        // A 401 is the one skip an operator can act on, so it says how.
        assert!(
            lines[2].contains("`newt auth stale`"),
            "the auth hint is the actionable half: {:?}",
            lines[2]
        );
        assert!(
            !lines[3].contains("newt auth"),
            "a timeout is not an auth problem: {:?}",
            lines[3]
        );
        assert!(lines[4].contains("disabled in config"), "{:?}", lines[4]);
    }

    /// A muted server reads as muted AND as still connected — the distinction
    /// `/mcp off` exists for, and the one a status line most easily loses.
    #[test]
    fn a_muted_server_still_says_it_is_connected() {
        let mut mcp = Mcp::empty();
        mcp.statuses = vec![(
            "github".to_string(),
            McpStatus::Connected {
                tools: 3,
                confinement: Confinement::from_sandbox(None),
                net: NetGate::from_posture(newt_mcp_client::NetPosture::Advisory),
            },
        )];
        assert!(mcp.status_lines()[1].contains("✓ connected"));

        mcp.mute("github");
        let muted = mcp.status_lines()[1].clone();
        assert!(muted.contains("⏸ muted this session"), "{muted}");
        assert!(
            muted.contains("still connected"),
            "muting removes tools, not the connection: {muted}"
        );
        assert!(
            muted.contains("/mcp on github"),
            "it names the way back: {muted}"
        );
    }

    /// No servers says so, and says where they would be configured — "none
    /// configured" and "the view is broken" must not look the same.
    #[test]
    fn no_servers_says_where_they_would_be_configured() {
        let lines = Mcp::empty().status_lines();
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("no MCP servers configured"),
            "{:?}",
            lines[0]
        );
        assert!(lines[0].contains("[[mcp_servers]]"), "{:?}", lines[0]);
    }
}
