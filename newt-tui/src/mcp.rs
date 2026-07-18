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

use newt_core::mcp::{McpServerEntry, TransportKind};
use newt_mcp_client::{connect_http, connect_stdio, namespaced, split_namespaced, ConnectedServer};
use serde_json::{json, Value};

/// Per-server launch outcome for the `/mcp` surface (#1149).
#[derive(Debug, Clone)]
pub(crate) enum McpStatus {
    /// Connected, with this many tools registered.
    Connected(usize),
    /// Skipped at launch (auth failure, timeout, spawn error, legacy SSE…).
    Skipped(String),
    /// `enabled = false` in config — not attempted.
    Disabled,
}

/// The session's connected MCP servers.
pub(crate) struct Mcp {
    /// (server name, launch outcome) for every DISCOVERED entry — the `/mcp`
    /// status table (#1149). Includes disabled + skipped servers.
    pub(crate) statuses: Vec<(String, McpStatus)>,
    servers: Vec<ConnectedServer>,
    /// When `true`, hyphens in server names are replaced with underscores in
    /// advertised tool names and routing lookups.  Matches the behaviour of
    /// API proxies that normalise tool-name characters.  Controlled by
    /// `[tui].sanitize_mcp_server_names` in the newt config (default: `true`).
    sanitize_server_names: bool,
}

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
fn parse_scheme_host(url: Option<&str>) -> (String, String) {
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

/// A loopback host — the dev exception that needs no https and emits no warning.
/// Whether an HTTP MCP server at `host` may be dialed under the session net
/// scope (#1156). Empty host (no URL) and loopback are always allowed (dev /
/// no-egress); any other host must be permitted by the net allow-list.
fn http_egress_permitted(net: &newt_core::caveats::Scope<String>, host: &str) -> bool {
    host.is_empty()
        || host_is_loopback(host)
        || newt_core::caveats::ScopeExt::permits(net, &host.to_string())
}

fn host_is_loopback(host: &str) -> bool {
    host == "localhost" || host == "::1" || host.starts_with("127.")
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
            .any(|h| h.eq_ignore_ascii_case(&host))
}

/// Inject the (optional) Bearer into `entry` per the transport policy, warning on
/// every non-loopback unencrypted connection. Mutates `entry.headers`.
fn apply_transport_security(
    entry: &mut McpServerEntry,
    token: Option<String>,
    allow_insecure_hosts: &[String],
) {
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
    if let (Some(token), true) = (token, allowed) {
        entry
            .headers
            .insert("Authorization".into(), format!("Bearer {token}"));
    }
}

impl Mcp {
    /// Remove a live server by name (`/mcp disable`, #1149): its tools leave
    /// the surface immediately; config persistence is the caller's job.
    pub(crate) fn drop_server(&mut self, name: &str) {
        self.servers.retain(|s| s.name != name);
    }

    /// An empty set — connects to nothing. Used by tests (the live session
    /// always builds via [`Self::connect`]).
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self {
            statuses: Vec::new(),
            servers: Vec::new(),
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
        let entries = newt_core::mcp::discover(
            cfg_servers,
            home.as_deref(),
            std::path::Path::new(workspace),
        );
        let mut servers = Vec::new();
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
                TransportKind::Stdio => connect_stdio(entry, caveats).await,
                TransportKind::Http => {
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
                    let mut enriched = entry.clone();
                    // Load the stored hermes OAuth token only when the operator
                    // hasn't already configured an explicit Authorization header.
                    let already_authed = enriched.headers.contains_key("Authorization")
                        || enriched.headers.contains_key("authorization");
                    let token = if already_authed {
                        None
                    } else {
                        crate::mcp_token::load_bearer_token(&entry.name).await
                    };
                    // Secure-by-default transport policy: WARN on any non-loopback
                    // unencrypted connection, and only inject the OAuth Bearer over
                    // https / loopback / an explicitly allow-listed host
                    // (docs/decisions/mcp_transport_security.md).
                    apply_transport_security(&mut enriched, token, allow_insecure_hosts);
                    connect_http(&enriched).await
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
                    statuses.push((
                        entry.name.clone(),
                        McpStatus::Connected(connected.tools.len()),
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
            .map(|s| (s.name.clone(), s.tools.len()))
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

    /// Whether `name` is a namespaced tool belonging to a connected server.
    ///
    /// Matches the sanitized form (hyphens → underscores in the server prefix)
    /// so that a tool advertised as `acme_server__X` routes to the server
    /// stored as `acme-server`.
    pub(crate) fn handles(&self, name: &str) -> bool {
        match split_namespaced(name) {
            Some((server, _)) => self
                .servers
                .iter()
                .any(|s| server_prefix(&s.name, self.sanitize_server_names) == server),
            None => false,
        }
    }

    /// Route a `server__tool` call to its server and render the result as the
    /// string the agent loop feeds back as the tool message.
    pub(crate) async fn call(&mut self, name: &str, args: &Value) -> String {
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
            // Scoped FR-14 (#1042): the result body is external data from the
            // connected server, not a newt-generated message — wrap it so the
            // model treats it as information, not instructions. `e` below is
            // OUR OWN connection-error text, not external content, so it is
            // NOT wrapped.
            Ok(result) => newt_core::wrap_untrusted(name, &format_result(&result)),
            Err(e) => format!("error: {e}"),
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
    async fn call(&mut self, name: &str, args: &Value) -> String {
        Self::call(self, name, args).await
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

    #[test]
    fn empty_handles_nothing_and_has_no_defs() {
        let mcp = Mcp::empty();
        assert!(mcp.is_empty());
        assert!(!mcp.handles("git__status"));
        assert!(mcp.tool_defs().is_empty());
    }

    // ── transport security: the OAuth Bearer must never go over plaintext ──

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
    fn apply_transport_security_withholds_token_over_plain_http() {
        // the blocker: a stored Bearer must NOT be injected over plaintext http
        let mut e = http_entry("http://api.maas.com/mcp");
        apply_transport_security(&mut e, Some("SECRET".into()), &[]);
        assert!(
            !e.headers.contains_key("Authorization"),
            "Bearer leaked over plaintext http: {:?}",
            e.headers
        );
    }

    #[test]
    fn apply_transport_security_injects_over_https_and_allowlisted() {
        let mut https = http_entry("https://api.maas.com/mcp");
        apply_transport_security(&mut https, Some("SECRET".into()), &[]);
        assert_eq!(
            https.headers.get("Authorization").map(String::as_str),
            Some("Bearer SECRET")
        );

        let mut allowed = http_entry("http://api.maas.com/mcp");
        apply_transport_security(
            &mut allowed,
            Some("SECRET".into()),
            &["api.maas.com".to_string()],
        );
        assert_eq!(
            allowed.headers.get("Authorization").map(String::as_str),
            Some("Bearer SECRET")
        );

        let mut loopback = http_entry("http://127.0.0.1:9/mcp");
        apply_transport_security(&mut loopback, Some("SECRET".into()), &[]);
        assert!(loopback.headers.contains_key("Authorization"));
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
}
