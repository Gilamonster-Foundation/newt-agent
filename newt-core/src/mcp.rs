//! Shared MCP server discovery.
//!
//! newt does not define its MCP servers in isolation. It **auto-discovers the
//! same servers you already configured for Claude Code** (so you do not
//! duplicate config) and merges in a newt-native `[[mcp_servers]]` section for
//! extras or overrides. This module only *resolves the merged list*; actually
//! connecting to the servers (the MCP client transport) is a separate layer.
//!
//! Sources, in precedence order (earlier wins on a name clash):
//! 1. newt's own `[[mcp_servers]]` (from the resolved `config.toml`)
//! 2. newt's user-owned `~/.newt/mcp.toml`
//! 3. Claude Code user config: `~/.claude.json` → `mcpServers`
//! 4. Project config: `<workspace>/.mcp.json` → `mcpServers`
//!
//! One [`McpServerEntry`] type is the common target for newt, Claude, and Codex
//! shapes. Borrowed Claude/Codex configuration is parsed permissively for
//! discovery but strictly for explicit adoption, where unsupported authority or
//! transport semantics must fail instead of being silently erased.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::agent_identity::{Secret, SecretRef};
use crate::error::{NewtError, Result};

/// Which transport an MCP server speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    /// Local subprocess speaking JSON-RPC over stdio — the common case, and the
    /// default when a Claude entry omits `type` but carries a `command`.
    #[default]
    Stdio,
    /// Server-sent-events HTTP endpoint.
    Sse,
    /// Streamable-HTTP endpoint.
    Http,
}

impl TransportKind {
    /// The lowercase config keyword for this transport (the `type` field).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Sse => "sse",
            Self::Http => "http",
        }
    }

    /// Parse a config keyword (`stdio` / `sse` / `http`) — the inverse of
    /// [`as_str`](Self::as_str). Keeps newt-core clap-free: the CLI's
    /// `--transport` value parser delegates here (the `ColorMode` pattern).
    #[must_use]
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s {
            "stdio" => Some(Self::Stdio),
            "sse" => Some(Self::Sse),
            "http" => Some(Self::Http),
            _ => None,
        }
    }
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Trust boundary on secret resolution (#1301 security review)
// ---------------------------------------------------------------------------

/// Whether a discovered [`McpServerEntry`] came from a **newt-owned** config
/// source or a **borrowed** Claude/project overlay — the trust boundary that
/// governs how its `env` / `headers` secrets resolve.
///
/// newt-owned config (a `[[mcp_servers]]` in `config.toml`, or `~/.newt/mcp.toml`)
/// is the operator's own machine config, exactly like a line in their shell
/// profile: it may name a command to run (`${cmd:…}` / `{ cmd = … }`), a file to
/// read (`${file:…}` / `{ file = … }`), or an env var, and newt resolves all of
/// it host-side.
///
/// A discovered Claude/project overlay (`~/.claude.json`,
/// `<workspace>/.mcp.json`) is attacker-reachable — a freshly cloned repo can
/// ship a hostile `.mcp.json`. So for an **untrusted** entry the literal
/// env/header values pass to the child **verbatim** (NO `${…}` interpolation, NO
/// `cmd:`/`file:` execution or read — the pre-#1301 behavior, which also restores
/// Claude-overlay compatibility), and a structured `{ env | file | cmd }`
/// reference is **rejected**: untrusted config must never be able to name a
/// command to run or a file to read on the host.
///
/// The marker is set at discovery ([`discover`] / [`parse_claude_mcp`]); it is
/// never serialized (it is provenance, not config the user writes) and defaults
/// to [`McpTrust::Trusted`] so an entry constructed in newt's own code
/// (`newt mcp add`/`install`/`probe`, catalog installs) is trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpTrust {
    /// newt-owned config — full `${…}` interpolation and `{env|file|cmd}` refs.
    #[default]
    Trusted,
    /// A borrowed Claude/project overlay — literals pass verbatim, refs rejected.
    Untrusted,
}

/// Resolve one `env` / `headers` value to its plaintext [`Secret`] under a trust
/// level — the single choke point for the #1301 trust boundary.
///
/// - [`McpTrust::Trusted`] (newt-owned config): the value is resolved fully via
///   [`SecretValue::resolve`] — `${…}` interpolation for a literal, the
///   `{env|file|cmd}` machinery for a reference.
/// - [`McpTrust::Untrusted`] (a discovered Claude/project overlay): a
///   [`SecretValue::Literal`] passes through **verbatim** (never interpolated,
///   so a `${cmd:…}` in an untrusted value is inert text, not a host command),
///   and a [`SecretValue::Ref`] is a hard error — untrusted config may not name
///   a command to run or a file to read.
pub fn resolve_secret_under_trust(value: &SecretValue, trust: McpTrust) -> Result<Secret> {
    match trust {
        McpTrust::Trusted => value.resolve(),
        McpTrust::Untrusted => match value {
            SecretValue::Literal(s) => Ok(Secret::new(s.clone())),
            SecretValue::Ref(_) => Err(NewtError::Config(
                "a discovered (untrusted) MCP config source (a project `.mcp.json` or \
                 `~/.claude.json`) may not use a `{ env | file | cmd }` secret reference — \
                 only newt-owned config (`config.toml`, `~/.newt/mcp.toml`) may name a command \
                 to run or a file to read. `newt mcp import` this server to adopt it as trusted."
                    .to_string(),
            )),
        },
    }
}

// ---------------------------------------------------------------------------
// MCP admission gate (the single spawn/dial/expose decision)
// ---------------------------------------------------------------------------

/// Why an MCP server was refused admission (i.e. must not be spawned, dialed,
/// or have its tools exposed to the model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionDenied {
    /// `enabled = false` — the operator turned it off. `enabled` is a
    /// visibility switch, not a trust decision, but a disabled entry is never
    /// admitted regardless of trust.
    Disabled,
    /// A discovered (untrusted) overlay — a repo `.mcp.json`, `~/.claude.json`,
    /// or a walked-up project `config.toml` — with no approval recorded OUTSIDE
    /// the repo. Untrusted config may not spawn a process, dial an endpoint, or
    /// expose tools until it is explicitly approved; no such approval record
    /// exists yet and headless has no interactive path, so it fails closed.
    UntrustedNotApproved,
}

impl std::fmt::Display for AdmissionDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "disabled (`enabled = false`)"),
            Self::UntrustedNotApproved => write!(
                f,
                "untrusted MCP config not approved — a discovered `.mcp.json` / \
                 `~/.claude.json` / project overlay may not spawn or dial without an \
                 approval recorded outside the repo; `newt mcp import` to adopt it as trusted"
            ),
        }
    }
}

/// An MCP server that PASSED [`admit`] — the unforgeable witness the transport
/// constructors require before they may spawn or dial. The inner reference is
/// private, so the ONLY way to obtain an `AdmittedServer` is through [`admit`];
/// and because the `newt-mcp-client` transport constructors (`StdioTransport::spawn`
/// and `HttpTransport::connect`) take `&AdmittedServer` — not a bare
/// `&McpServerEntry` — a spawn/dial of an un-admitted server does not type-check.
/// This guarantee is **structural**, not by-convention: step-1.2 (#1562 follow-up)
/// sealed the constructors so no call site — in-crate or downstream — can reach a
/// spawn without the witness. `entry()` hands back a read-only `&McpServerEntry`
/// for the transport to build against, which cannot reopen the bypass (the
/// constructors want the witness, not the entry).
#[derive(Debug, Clone, Copy)]
pub struct AdmittedServer<'a> {
    entry: &'a McpServerEntry,
}

impl<'a> AdmittedServer<'a> {
    /// The admitted entry, read-only, for the transport to spawn/dial against.
    pub fn entry(&self) -> &'a McpServerEntry {
        self.entry
    }
}

/// The single MCP admission gate: decide whether a configured server may be
/// connected — spawned, dialed, and its tools exposed — at all. This is where
/// `enabled ≠ trusted ≠ approved` separate: a disabled entry is refused; an
/// untrusted overlay fails closed (no approval-record type exists yet, and
/// headless has no interactive approval path); only a trusted, enabled entry is
/// admitted, returning an [`AdmittedServer`] witness the transport layer
/// requires. Both the TUI and the headless connection planners route through
/// here, so admission is decided in ONE place rather than two divergent loops.
pub fn admit(entry: &McpServerEntry) -> std::result::Result<AdmittedServer<'_>, AdmissionDenied> {
    if !entry.enabled {
        return Err(AdmissionDenied::Disabled);
    }
    match entry.trust {
        McpTrust::Trusted => Ok(AdmittedServer { entry }),
        McpTrust::Untrusted => Err(AdmissionDenied::UntrustedNotApproved),
    }
}

// ---------------------------------------------------------------------------
// Secret-bearing MCP config values (`env` / `headers`)
// ---------------------------------------------------------------------------

/// The value of one `env` or `headers` entry on an [`McpServerEntry`].
///
/// Two shapes, distinguished structurally (serde `untagged`) so config stays
/// backward-compatible and Claude-Code-interoperable:
///
/// - a **plain string** deserializes to [`SecretValue::Literal`] (a bare Claude
///   `"env": { "TOKEN": "abc" }` value, or a newt literal). A literal may embed
///   `${...}` interpolation tokens (see [`interpolate`]) — including Claude's
///   `${VAR}` — resolved host-side at spawn.
/// - a **table** (`{ env = … }` / `{ file = … }` / `{ cmd = … }`) deserializes to
///   [`SecretValue::Ref`], the existing [`SecretRef`] secret-by-reference scheme,
///   for a value that is wholly a secret.
///
/// Both resolve, host-side and just before the confined spawn, into a redacting
/// [`Secret`] via [`SecretValue::resolve`] — so plaintext never lives in
/// `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SecretValue {
    /// A literal string (may carry `${...}` interpolation tokens).
    Literal(String),
    /// A structured secret reference (`{ env | file | cmd }`).
    Ref(SecretRef),
}

impl SecretValue {
    /// Construct a literal value.
    pub fn literal(value: impl Into<String>) -> Self {
        Self::Literal(value.into())
    }

    /// Borrow the literal string, if this is a [`SecretValue::Literal`]. A
    /// [`SecretValue::Ref`] returns `None` (its value is not known until
    /// resolved).
    #[must_use]
    pub fn as_literal(&self) -> Option<&str> {
        match self {
            Self::Literal(s) => Some(s),
            Self::Ref(_) => None,
        }
    }

    /// Resolve this value to its [`Secret`], host-side.
    ///
    /// A [`SecretValue::Literal`] is `${...}`-interpolated (a token-free literal
    /// is returned verbatim); a [`SecretValue::Ref`] is resolved through
    /// [`SecretRef::resolve`]. A reference that resolves to nothing (missing env
    /// var / empty file / empty command output) is a hard error — a missing
    /// secret fails loudly at spawn, never silently empty.
    pub fn resolve(&self) -> Result<Secret> {
        match self {
            Self::Literal(s) => Ok(Secret::new(interpolate(s)?)),
            Self::Ref(r) => r.resolve()?.ok_or_else(|| {
                NewtError::Config(
                    "MCP secret reference resolved to nothing (missing env var, empty file, \
                     or empty command output)"
                        .to_string(),
                )
            }),
        }
    }
}

/// One `${...}` interpolation token, classified by scheme.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InterpToken {
    /// `${VAR}` or `${env:VAR}` — an environment variable.
    Env(String),
    /// `${file:PATH}` — first non-empty line of a (tilde-expanded) file.
    File(String),
    /// `${cmd:COMMAND}` — trimmed stdout of a shell command (the Vault path).
    Cmd(String),
}

impl InterpToken {
    /// Map onto the existing [`SecretRef`] resolver — one scheme, not two.
    fn to_secret_ref(&self) -> SecretRef {
        match self {
            Self::Env(v) => SecretRef {
                env: Some(v.clone()),
                ..Default::default()
            },
            Self::File(p) => SecretRef {
                file: Some(p.clone()),
                ..Default::default()
            },
            Self::Cmd(c) => SecretRef {
                cmd: Some(c.clone()),
                ..Default::default()
            },
        }
    }

    /// A redaction-safe description (the reference, never a value) for errors.
    fn describe(&self) -> String {
        match self {
            Self::Env(v) => format!("${{env:{v}}}"),
            Self::File(p) => format!("${{file:{p}}}"),
            Self::Cmd(c) => format!("${{cmd:{c}}}"),
        }
    }
}

/// Whether `s` is a valid bare env-var reference for `${NAME}` — an identifier
/// `^[A-Za-z_][A-Za-z0-9_]*$`. This is the user's intended inline form (e.g.
/// `Bearer ${MY_TOKEN}`); anything else inside `${…}` is left verbatim.
fn is_env_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Classify the text inside a `${...}` token. Pure. `Some` only for a token newt
/// actually interpolates: a known scheme (`env:` / `file:` / `cmd:`) or a bare
/// `${NAME}` where `NAME` is a valid identifier. `None` for anything else —
/// a colon without a known scheme (`${VAR:-default}`), a non-identifier
/// (`${.field}`) — which the caller then passes through **verbatim** (a
/// conservative contract, #1301: an unrecognized `${…}` is NOT an error, so a
/// pre-existing literal that merely contains `${…}` keeps working).
fn classify_token(inner: &str) -> Option<InterpToken> {
    match inner.split_once(':') {
        Some(("env", rest)) => Some(InterpToken::Env(rest.to_string())),
        Some(("file", rest)) => Some(InterpToken::File(rest.to_string())),
        Some(("cmd", rest)) => Some(InterpToken::Cmd(rest.to_string())),
        // A colon with an unknown scheme (`${VAR:-default}`, `${x:y}`) is NOT a
        // newt token — pass it through verbatim, never a hard error.
        Some(_) => None,
        // No colon: a bare `${NAME}` interpolates only when NAME is a valid
        // identifier; otherwise (`${.field}`) it is verbatim.
        None => is_env_identifier(inner).then(|| InterpToken::Env(inner.to_string())),
    }
}

/// The pure core of [`interpolate`]: split `template` into literal runs and
/// `${...}` tokens, resolving each RECOGNIZED token through the injected
/// `resolve`. Literal text around tokens — and any UNRECOGNIZED `${…}` (unknown
/// scheme, non-identifier) — is preserved **verbatim** (the #1301 conservative
/// contract: an unrecognized `${…}` is never a hard error). `$${` is an escape
/// yielding a literal `${` (so an operator can express a literal `${`). An
/// unterminated `${` is the one hard error, and its message references NO value
/// (redaction-safe, #1301). Kept generic over the resolver so the
/// parsing/reassembly is unit-tested with literals — no env/fs/subprocess.
fn interpolate_with<F>(template: &str, resolve: F) -> Result<String>
where
    F: Fn(&InterpToken) -> Result<String>,
{
    // Fast path: the overwhelmingly common literal (a path, a log level, an
    // org id) carries no `${` and is returned byte-for-byte. (`$${` contains
    // `${`, so an escaped value correctly falls through to the scanner.)
    if !template.contains("${") {
        return Ok(template.to_string());
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        // `$${` escape: emit a literal `${` and resume AFTER it, so the brace
        // that follows is treated as ordinary text, not a token opener.
        if start >= 1 && rest.as_bytes()[start - 1] == b'$' {
            out.push_str(&rest[..start - 1]);
            out.push_str("${");
            rest = &rest[start + 2..];
            continue;
        }
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // Redaction-safe: reference the shape of the error, NEVER the value
            // (which may carry literal secret material before the stray `${`).
            return Err(NewtError::Config(
                "unterminated `${` in an MCP env/header value (missing closing `}`)".to_string(),
            ));
        };
        let inner = &after[..end];
        match classify_token(inner) {
            Some(token) => out.push_str(&resolve(&token)?),
            // Not a newt token — reassemble the `${…}` verbatim.
            None => {
                out.push_str("${");
                out.push_str(inner);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// The live token resolver — reads env / file / command via [`SecretRef`],
/// host-side. A token that resolves to nothing is a hard error (fail loud).
fn resolve_token_live(token: &InterpToken) -> Result<String> {
    match token.to_secret_ref().resolve()? {
        Some(secret) => Ok(secret.expose().to_string()),
        None => Err(NewtError::Config(format!(
            "{} resolved to nothing (missing env var, empty file, or empty command output)",
            token.describe()
        ))),
    }
}

/// Resolve every `${...}` token in `template`, host-side, preserving the literal
/// text around each token. Schemes: `${VAR}` / `${env:VAR}` (env var),
/// `${file:PATH}` (first non-empty line of the tilde-expanded file),
/// `${cmd:COMMAND}` (trimmed stdout of the command — the Vault path). A missing
/// or empty reference is a hard error.
///
/// SECURITY: a `${cmd:...}` token runs a program host-side, at the operator's
/// own trust level. This is only ever reached for **newt-owned (TRUSTED)** config
/// (see [`resolve_secret_under_trust`] / [`McpTrust`]) — config the operator
/// authored, exactly like a line in their shell profile. A borrowed
/// Claude/project overlay is UNTRUSTED and never reaches interpolation (its
/// literals pass verbatim, its refs are rejected), so a hostile `.mcp.json`
/// cannot smuggle a `${cmd:…}` onto the host. Resolution happens in newt's own
/// (unconfined) process, just before the child env / HTTP headers are built, and
/// the result is wrapped in [`Secret`]; it is never written back to config and
/// never enters newt's own process env.
pub fn interpolate(template: &str) -> Result<String> {
    interpolate_with(template, resolve_token_live)
}

/// One discovered MCP server, in a shape that parses from both Claude Code's
/// `mcpServers` JSON entries and newt's `[[mcp_servers]]` TOML tables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerEntry {
    /// Server name. In newt TOML this is the `name` field; for a Claude entry it
    /// is injected from the `mcpServers` map key (see [`parse_claude_mcp`]).
    #[serde(default)]
    pub name: String,

    /// Whether this server is connected at launch (`/mcp enable|disable` —
    /// #1149). Default true; a disabled entry stays in config, shows in
    /// `/mcp` as disabled, and costs nothing at startup.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Transport. Defaults to [`TransportKind::Stdio`] when absent.
    #[serde(default, rename = "type")]
    pub transport: TransportKind,

    /// stdio: the executable to spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// stdio: arguments to the executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// stdio: extra environment for the child. Each value is a [`SecretValue`]
    /// (a literal — possibly `${...}`-interpolated — or a `{ env | file | cmd }`
    /// reference), resolved host-side at spawn.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, SecretValue>,

    /// sse/http: the endpoint URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// sse/http: extra request headers. Each value is a [`SecretValue`] (a
    /// literal — possibly `${...}`-interpolated — or a `{ env | file | cmd }`
    /// reference), resolved host-side at connect.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, SecretValue>,

    /// Per-request timeout override, in seconds. `None` ⇒ the client's default
    /// (`newt_mcp_client::DEFAULT_REQUEST_TIMEOUT`). Raise it for a server whose
    /// tools legitimately run long — e.g. a routine engine that fans out across
    /// many repos and live APIs in a single `tools/call` — so the client does
    /// not give up on a call that is still making progress. The client clamps
    /// the resolved value to a sane ceiling. Accepts `requestTimeoutSecs` in
    /// Claude-format JSON.
    #[serde(
        default,
        alias = "requestTimeoutSecs",
        skip_serializing_if = "Option::is_none"
    )]
    pub request_timeout_secs: Option<u64>,

    /// Provenance / trust marker (#1301): whether this entry came from newt-owned
    /// config (TRUSTED — full secret resolution) or a borrowed Claude/project
    /// overlay (UNTRUSTED — literals verbatim, refs rejected). Set at discovery
    /// ([`discover`] / [`parse_claude_mcp`]); **never serialized** (`#[serde(skip)]`)
    /// and defaults to [`McpTrust::Trusted`] — see [`McpTrust`].
    #[serde(skip)]
    pub trust: McpTrust,
}

impl McpServerEntry {
    /// Whether this entry has the fields its transport requires. An invalid
    /// entry (e.g. a stdio server with no `command`) is dropped during discovery
    /// rather than silently producing a server that can never connect.
    pub fn is_valid(&self) -> bool {
        match self.transport {
            TransportKind::Stdio => self.command.is_some(),
            TransportKind::Sse | TransportKind::Http => self.url.is_some(),
        }
    }
}

/// Validate an entry for a comment-preserving write — shared by the config's
/// `[[mcp_servers]]` writer and the catalog's `[[servers]]` writer. An empty
/// name can never be addressed again; an entry failing [`McpServerEntry::is_valid`]
/// could never connect.
pub(crate) fn validate_entry_for_write(entry: &McpServerEntry) -> crate::error::Result<()> {
    if entry.name.trim().is_empty() {
        return Err(crate::error::NewtError::Config(
            "MCP server name cannot be empty".to_string(),
        ));
    }
    if !entry.is_valid() {
        let need = match entry.transport {
            TransportKind::Stdio => "a `command`",
            TransportKind::Sse | TransportKind::Http => "a `url`",
        };
        return Err(crate::error::NewtError::Config(format!(
            "a {} MCP server requires {need}",
            entry.transport.as_str()
        )));
    }
    Ok(())
}

/// Render an entry as a `toml_edit` table — the shape both writers append.
/// `description` (the catalog form) lands right after `name`. Defaults stay
/// implicit (no `enabled = true`, no `type = "stdio"`) so files stay minimal.
pub(crate) fn entry_to_toml_table(
    entry: &McpServerEntry,
    description: Option<&str>,
) -> crate::error::Result<toml_edit::Table> {
    let mut table = toml_edit::Table::new();
    table["name"] = toml_edit::value(&entry.name);
    if let Some(description) = description {
        table["description"] = toml_edit::value(description);
    }
    if !entry.enabled {
        table["enabled"] = toml_edit::value(false);
    }
    if entry.transport != TransportKind::Stdio {
        table["type"] = toml_edit::value(entry.transport.as_str());
    }
    if let Some(command) = &entry.command {
        table["command"] = toml_edit::value(command);
    }
    if !entry.args.is_empty() {
        table["args"] = toml_edit::value(toml_edit::Array::from_iter(&entry.args));
    }
    if !entry.env.is_empty() {
        table["env"] = toml_edit::value(secret_map_to_inline_table(&entry.env));
    }
    if let Some(url) = &entry.url {
        table["url"] = toml_edit::value(url);
    }
    if !entry.headers.is_empty() {
        table["headers"] = toml_edit::value(secret_map_to_inline_table(&entry.headers));
    }
    if let Some(secs) = entry.request_timeout_secs {
        table["request_timeout_secs"] = toml_edit::value(i64::try_from(secs).map_err(|_| {
            crate::error::NewtError::Config(format!("request timeout {secs}s is out of range"))
        })?);
    }
    Ok(table)
}

/// Render a `SecretValue` as a `toml_edit` value: a literal becomes a string, a
/// reference becomes an inline table with only its set `{ env | file | cmd }`
/// key — the inverse of the `untagged` deserialize, so a config round-trips.
fn secret_value_to_toml(value: &SecretValue) -> toml_edit::Value {
    match value {
        SecretValue::Literal(s) => toml_edit::Value::from(s.as_str()),
        SecretValue::Ref(r) => {
            let mut table = toml_edit::InlineTable::new();
            if let Some(env) = &r.env {
                table.insert("env", env.as_str().into());
            }
            if let Some(file) = &r.file {
                table.insert("file", file.as_str().into());
            }
            if let Some(cmd) = &r.cmd {
                table.insert("cmd", cmd.as_str().into());
            }
            toml_edit::Value::InlineTable(table)
        }
    }
}

/// Render an `env` / `headers` map as one inline table (`{ K = V, … }`).
fn secret_map_to_inline_table(map: &BTreeMap<String, SecretValue>) -> toml_edit::InlineTable {
    let mut table = toml_edit::InlineTable::new();
    for (k, v) in map {
        table.insert(k, secret_value_to_toml(v));
    }
    table
}

/// Parse the `mcpServers` object out of a Claude Code config value
/// (`~/.claude.json` or a project `.mcp.json`). The server name is taken from
/// each map key. Unparseable entries are skipped, not fatal.
pub fn parse_claude_mcp(value: &serde_json::Value) -> Vec<McpServerEntry> {
    let Some(map) = value
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(name, entry)| {
            let mut parsed: McpServerEntry = serde_json::from_value(entry.clone()).ok()?;
            // The name lives in the map key, not the entry body.
            parsed.name = name.clone();
            // A Claude/project overlay is borrowed, attacker-reachable config —
            // mark it UNTRUSTED at the single parse funnel so its secrets never
            // interpolate / execute a `${cmd:…}` or `{ cmd = … }` (#1301).
            parsed.trust = McpTrust::Untrusted;
            Some(parsed)
        })
        .collect()
}

/// A value-independent reason an entry in a borrowed MCP configuration cannot
/// be adopted. Reasons deliberately never include source text, field names, or
/// values: import diagnostics are allowed to identify the selected server, but
/// must not echo credentials from a file that has not crossed the trust
/// boundary yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpImportIssue {
    /// The source entry contains a field newt cannot preserve.
    UnknownField,
    /// The entry is not an object/table or has values of the wrong shape.
    InvalidShape,
    /// Transport or authority semantics cannot be preserved safely.
    UnsupportedSemantics,
}

impl std::fmt::Display for McpImportIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnknownField => "unsupported fields",
            Self::InvalidShape => "an invalid entry shape",
            Self::UnsupportedSemantics => "unsupported or ambiguous transport semantics",
        })
    }
}

/// One independently rejected entry. `name` is absent when the source's
/// top-level MCP container itself has the wrong shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpImportRejection {
    pub name: Option<String>,
    pub issue: McpImportIssue,
}

/// Strict import parse result. Discovery remains best-effort and permissive;
/// explicit adoption consumes this report and must account for every entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpImportParseReport {
    pub entries: Vec<McpServerEntry>,
    pub rejected: Vec<McpImportRejection>,
}

/// Secret-safe syntax error for borrowed Codex TOML. The original TOML parser
/// error embeds the complete offending line, so it must never cross the CLI
/// diagnostic boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpImportSyntaxError;

impl std::fmt::Display for McpImportSyntaxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid configuration syntax")
    }
}

impl std::error::Error for McpImportSyntaxError {}

/// Value-independent reason an HTTP MCP URL cannot cross the explicit import
/// boundary. The variants deliberately carry no source text so callers can
/// report failures without echoing credentials embedded in a borrowed URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpHttpUrlIssue {
    Invalid,
    UnsupportedScheme,
    UserInfo,
    Query,
    Fragment,
    MissingHost,
}

impl std::fmt::Display for McpHttpUrlIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Invalid => "an invalid URL",
            Self::UnsupportedScheme => "an unsupported URL scheme",
            Self::UserInfo => "URL userinfo",
            Self::Query => "a URL query",
            Self::Fragment => "a URL fragment",
            Self::MissingHost => "a URL without a hostname",
        })
    }
}

impl std::error::Error for McpHttpUrlIssue {}

/// One canonical representation used by import persistence and net grants.
/// Runtime consumers should use the same host value rather than reparsing with
/// transport-specific rules. IPv6 brackets are removed from `host`; domains are
/// lowercased and IDNA-normalized by `Url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalMcpHttpUrl {
    pub url: String,
    pub host: String,
}

/// Validate and canonicalize a remotely imported MCP URL. Imported URLs are
/// control-plane authority, not arbitrary web links: userinfo, every query, and
/// every fragment are rejected fail-closed because newt cannot prove those
/// components are credential-free.
pub fn canonical_mcp_http_url(
    raw: &str,
) -> std::result::Result<CanonicalMcpHttpUrl, McpHttpUrlIssue> {
    let parsed = reqwest::Url::parse(raw).map_err(|_| McpHttpUrlIssue::Invalid)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(McpHttpUrlIssue::UnsupportedScheme);
    }
    let raw_authority_has_userinfo = raw
        .split_once("://")
        .and_then(|(_, rest)| rest.split(['/', '?', '#']).next())
        .is_some_and(|authority| authority.contains('@'));
    if raw_authority_has_userinfo || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(McpHttpUrlIssue::UserInfo);
    }
    if parsed.query().is_some() {
        return Err(McpHttpUrlIssue::Query);
    }
    if parsed.fragment().is_some() {
        return Err(McpHttpUrlIssue::Fragment);
    }
    let host = parsed
        .host_str()
        .filter(|host| !host.is_empty())
        .ok_or(McpHttpUrlIssue::MissingHost)?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    Ok(CanonicalMcpHttpUrl {
        url: parsed.to_string(),
        host,
    })
}

fn portable_env_names<T>(values: &BTreeMap<String, T>) -> bool {
    let mut canonical = std::collections::BTreeSet::new();
    values
        .keys()
        .all(|name| is_safe_env_var_name(name) && canonical.insert(name.to_ascii_uppercase()))
}

fn portable_header_names<T>(values: &BTreeMap<String, T>) -> bool {
    let mut canonical = std::collections::BTreeSet::new();
    values.keys().all(|name| {
        is_safe_http_header_name(name)
            && !is_transport_owned_mcp_header(name)
            && canonical.insert(name.to_ascii_lowercase())
    })
}

/// Headers owned by the streamable-HTTP MCP transport. Allowing an imported
/// connector to configure any of these would either change the request origin
/// or conflict with the session/protocol values negotiated by the client.
#[must_use]
pub fn is_transport_owned_mcp_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "host" | "mcp-protocol-version" | "mcp-session-id"
    )
}

/// Strict, import-only Claude parser. Unlike discovery, adoption rejects
/// unknown fields and ambiguous transport shapes instead of silently erasing
/// semantics before stamping an entry trusted.
#[must_use]
pub fn parse_claude_mcp_for_import(value: &serde_json::Value) -> McpImportParseReport {
    const SUPPORTED_FIELDS: &[&str] = &[
        "enabled",
        "type",
        "command",
        "args",
        "env",
        "url",
        "headers",
        "request_timeout_secs",
        "requestTimeoutSecs",
    ];

    let Some(servers) = value.get("mcpServers") else {
        return McpImportParseReport::default();
    };
    let Some(servers) = servers.as_object() else {
        return McpImportParseReport {
            entries: Vec::new(),
            rejected: vec![McpImportRejection {
                name: None,
                issue: McpImportIssue::InvalidShape,
            }],
        };
    };

    let mut report = McpImportParseReport::default();
    for (name, raw) in servers {
        let Some(object) = raw.as_object() else {
            report.rejected.push(McpImportRejection {
                name: Some(name.clone()),
                issue: McpImportIssue::InvalidShape,
            });
            continue;
        };
        if object
            .keys()
            .any(|field| !SUPPORTED_FIELDS.contains(&field.as_str()))
        {
            report.rejected.push(McpImportRejection {
                name: Some(name.clone()),
                issue: McpImportIssue::UnknownField,
            });
            continue;
        }
        let Ok(mut entry) = serde_json::from_value::<McpServerEntry>(raw.clone()) else {
            report.rejected.push(McpImportRejection {
                name: Some(name.clone()),
                issue: McpImportIssue::InvalidShape,
            });
            continue;
        };
        entry.name = name.clone();
        entry.trust = McpTrust::Untrusted;
        let semantics_preserved = match entry.transport {
            TransportKind::Stdio => {
                entry.command.is_some()
                    && entry.url.is_none()
                    && entry.headers.is_empty()
                    && portable_env_names(&entry.env)
            }
            TransportKind::Http => {
                entry.url.is_some()
                    && entry.command.is_none()
                    && entry.args.is_empty()
                    && entry.env.is_empty()
                    && portable_header_names(&entry.headers)
            }
            // Newt's runtime deliberately skips legacy SSE, so adopting one
            // would report success for a connector that can never run.
            TransportKind::Sse => false,
        };
        if semantics_preserved {
            report.entries.push(entry);
        } else {
            report.rejected.push(McpImportRejection {
                name: Some(name.clone()),
                issue: McpImportIssue::UnsupportedSemantics,
            });
        }
    }
    report
}

/// Parse Codex's `[mcp_servers.<name>]` TOML tables into newt's shared MCP
/// representation.
///
/// Codex selects transport by shape rather than a `type` field: `url` is
/// streamable HTTP, while `command` (with optional `args`) is stdio. Entries
/// that mix the two shapes, contain fields newt cannot preserve without
/// widening access, or use an unknown field are dropped independently.
///
/// This parser deliberately imports credential *references*, never credential
/// values. `bearer_token_env_var` becomes an `Authorization` interpolation and
/// each `env_http_headers` value becomes a structured [`SecretRef`] environment
/// reference. Local `env_vars` are forwarded through the same reference type.
/// Literal `http_headers` and stdio `env` cannot be imported because Codex may
/// store plaintext credentials there. `tool_timeout_sec` maps to newt's
/// per-request timeout; `auth = "oauth"` is accepted because Newt performs
/// OAuth discovery. Startup timeouts are rejected because Newt cannot currently
/// preserve their failure contract.
/// Every result is marked
/// [`McpTrust::Untrusted`], matching other borrowed configuration; an explicit
/// `newt mcp import` is required before its process or endpoint may be used.
#[must_use]
pub fn parse_codex_mcp_toml(text: &str) -> Vec<McpServerEntry> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SourcedCodexEnvVar {
        name: String,
        #[serde(default)]
        source: Option<String>,
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum CodexEnvVar {
        Name(String),
        Sourced(SourcedCodexEnvVar),
    }

    impl CodexEnvVar {
        fn local_name(self) -> Option<String> {
            match self {
                Self::Name(name) => Some(name),
                Self::Sourced(SourcedCodexEnvVar { name, source }) => match source.as_deref() {
                    None | Some("local") => Some(name),
                    _ => None,
                },
            }
        }
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CodexEntry {
        url: Option<String>,
        command: Option<String>,
        args: Option<Vec<String>>,
        env: Option<BTreeMap<String, String>>,
        bearer_token_env_var: Option<String>,
        http_headers: Option<BTreeMap<String, String>>,
        env_http_headers: Option<BTreeMap<String, String>>,
        env_vars: Option<Vec<CodexEnvVar>>,
        auth: Option<String>,
        startup_timeout_sec: Option<u64>,
        tool_timeout_sec: Option<u64>,
        required: Option<bool>,
        enabled: Option<bool>,
    }

    let Ok(document) = toml::from_str::<toml::Value>(text) else {
        return Vec::new();
    };
    let Some(servers) = document.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Vec::new();
    };

    servers
        .iter()
        .filter_map(|(name, value)| {
            if name.trim().is_empty() {
                return None;
            }
            let parsed: CodexEntry = value.clone().try_into().ok()?;
            let CodexEntry {
                url,
                command,
                args,
                env,
                bearer_token_env_var,
                http_headers,
                env_http_headers,
                env_vars,
                auth,
                startup_timeout_sec,
                tool_timeout_sec,
                required,
                enabled,
            } = parsed;

            // Newt discovers OAuth automatically, so Codex's default `oauth`
            // intent carries over without storing an extra field. ChatGPT
            // session auth, required-startup semantics, and a per-connector
            // startup timeout cannot be preserved; accepting any of them would
            // silently change authority or failure behavior.
            if auth.as_deref().is_some_and(|kind| kind != "oauth")
                || required.unwrap_or(false)
                || startup_timeout_sec.is_some()
            {
                return None;
            }

            match (url, command) {
                (Some(url), None) => {
                    // `args` and `env` are stdio-only. Their presence alongside
                    // a URL is an ambiguous transport, even when empty.
                    if args.is_some()
                        || env.is_some()
                        || env_vars.is_some()
                        || url.trim().is_empty()
                    {
                        return None;
                    }
                    if http_headers
                        .as_ref()
                        .is_some_and(|values| !portable_header_names(values))
                    {
                        return None;
                    }

                    let mut headers = BTreeMap::new();
                    let mut canonical_header_names = std::collections::BTreeSet::new();
                    for (header, env_var) in env_http_headers.unwrap_or_default() {
                        if !is_safe_http_header_name(&header)
                            || is_transport_owned_mcp_header(&header)
                            || !is_safe_env_var_name(&env_var)
                            || !canonical_header_names.insert(header.to_ascii_lowercase())
                        {
                            return None;
                        }
                        headers.insert(
                            header,
                            SecretValue::Ref(SecretRef {
                                env: Some(env_var),
                                ..Default::default()
                            }),
                        );
                    }
                    if let Some(env_var) = bearer_token_env_var {
                        if !is_safe_env_var_name(&env_var)
                            || !canonical_header_names.insert("authorization".to_string())
                        {
                            return None;
                        }
                        headers.insert(
                            "Authorization".to_string(),
                            SecretValue::literal(format!("Bearer ${{env:{env_var}}}")),
                        );
                    }

                    Some(McpServerEntry {
                        name: name.clone(),
                        enabled: enabled.unwrap_or(true),
                        transport: TransportKind::Http,
                        command: None,
                        args: Vec::new(),
                        env: BTreeMap::new(),
                        url: Some(url),
                        headers,
                        request_timeout_secs: tool_timeout_sec,
                        trust: McpTrust::Untrusted,
                    })
                }
                (None, Some(command)) => {
                    // HTTP credential fields are invalid on a stdio entry. The
                    // literal stdio `env` map itself is accepted but omitted:
                    // there is no safe way to distinguish configuration from a
                    // plaintext token in Codex's string-to-string map.
                    if bearer_token_env_var.is_some()
                        || http_headers.is_some()
                        || env_http_headers.is_some()
                        || auth.is_some()
                        || command.trim().is_empty()
                    {
                        return None;
                    }
                    if env
                        .as_ref()
                        .is_some_and(|values| !portable_env_names(values))
                    {
                        return None;
                    }
                    let mut forwarded_env = BTreeMap::new();
                    let mut canonical_env_names = std::collections::BTreeSet::new();
                    for env_var in env_vars.unwrap_or_default() {
                        let name = env_var.local_name()?;
                        if !is_safe_env_var_name(&name)
                            || !canonical_env_names.insert(name.to_ascii_uppercase())
                        {
                            return None;
                        }
                        forwarded_env.insert(
                            name.clone(),
                            SecretValue::Ref(SecretRef {
                                env: Some(name),
                                ..Default::default()
                            }),
                        );
                    }

                    Some(McpServerEntry {
                        name: name.clone(),
                        enabled: enabled.unwrap_or(true),
                        transport: TransportKind::Stdio,
                        command: Some(command),
                        args: args.unwrap_or_default(),
                        env: forwarded_env,
                        url: None,
                        headers: BTreeMap::new(),
                        request_timeout_secs: tool_timeout_sec,
                        trust: McpTrust::Untrusted,
                    })
                }
                // Neither transport, or both transports at once.
                _ => None,
            }
        })
        .collect()
}

/// Strict Codex parser for explicit adoption. The discovery parser above keeps
/// its best-effort behavior; this wrapper accounts for every source entry and
/// converts syntax failures to a source-text-free error.
pub fn parse_codex_mcp_toml_for_import(
    text: &str,
) -> std::result::Result<McpImportParseReport, McpImportSyntaxError> {
    const SUPPORTED_FIELDS: &[&str] = &[
        "url",
        "command",
        "args",
        "env",
        "bearer_token_env_var",
        "http_headers",
        "env_http_headers",
        "env_vars",
        "auth",
        "startup_timeout_sec",
        "tool_timeout_sec",
        "required",
        "enabled",
    ];

    let document = toml::from_str::<toml::Value>(text).map_err(|_| McpImportSyntaxError)?;
    let Some(raw_servers) = document.get("mcp_servers") else {
        return Ok(McpImportParseReport::default());
    };
    let Some(raw_servers) = raw_servers.as_table() else {
        return Ok(McpImportParseReport {
            entries: Vec::new(),
            rejected: vec![McpImportRejection {
                name: None,
                issue: McpImportIssue::InvalidShape,
            }],
        });
    };

    let entries = parse_codex_mcp_toml(text);
    let accepted: std::collections::BTreeSet<&str> =
        entries.iter().map(|entry| entry.name.as_str()).collect();
    let mut rejected = Vec::new();
    for (name, raw) in raw_servers {
        if accepted.contains(name.as_str()) {
            continue;
        }
        let issue = match raw.as_table() {
            None => McpImportIssue::InvalidShape,
            Some(table)
                if table
                    .keys()
                    .any(|field| !SUPPORTED_FIELDS.contains(&field.as_str())) =>
            {
                McpImportIssue::UnknownField
            }
            Some(_) => McpImportIssue::UnsupportedSemantics,
        };
        rejected.push(McpImportRejection {
            name: Some(name.clone()),
            issue,
        });
    }
    Ok(McpImportParseReport { entries, rejected })
}

/// Count fields omitted by [`parse_codex_mcp_toml`] because Codex represents
/// their values as plaintext strings. Keys and values are never copied into this
/// result, so an importer can fail loudly without trusting source-controlled
/// diagnostic text.
#[must_use]
pub fn codex_mcp_omitted_field_counts(text: &str) -> BTreeMap<String, usize> {
    let Ok(document) = toml::from_str::<toml::Value>(text) else {
        return BTreeMap::new();
    };
    let Some(servers) = document.get("mcp_servers").and_then(toml::Value::as_table) else {
        return BTreeMap::new();
    };

    let mut omitted = BTreeMap::new();
    for (name, value) in servers {
        let Some(table) = value.as_table() else {
            continue;
        };
        let mut count = 0;
        for source in ["env", "http_headers"] {
            let Some(values) = table.get(source).and_then(toml::Value::as_table) else {
                continue;
            };
            count += values.len();
        }
        if count > 0 {
            omitted.insert(name.clone(), count);
        }
    }
    omitted
}

/// Restrict imported environment references to portable shell variable names.
fn is_safe_env_var_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|b| matches!(b, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// RFC 9110 `token` syntax used for HTTP field names.
fn is_safe_http_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Read + parse a Claude-format MCP config file. Missing or malformed files
/// yield an empty list (discovery is best-effort, never fatal).
fn load_claude_file(path: &Path) -> Vec<McpServerEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    parse_claude_mcp(&value)
}

/// Parse a newt-owned `mcp.toml` document — a bare `[[mcp_servers]]` array in
/// the exact same schema as `config.toml`'s section. Best-effort: a malformed
/// document yields an empty list (discovery is never fatal — mirrors
/// [`load_claude_file`]). Pure.
pub fn parse_newt_mcp_toml(text: &str) -> Vec<McpServerEntry> {
    #[derive(Deserialize, Default)]
    struct Doc {
        #[serde(default)]
        mcp_servers: Vec<McpServerEntry>,
    }
    toml::from_str::<Doc>(text)
        .map(|d| d.mcp_servers)
        .unwrap_or_default()
}

/// Read + parse a newt-owned `~/.newt/mcp.toml`. Missing or malformed files
/// yield an empty list (best-effort, never fatal).
fn load_newt_mcp_toml(path: &Path) -> Vec<McpServerEntry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_newt_mcp_toml(&text)
}

/// The exact server prefix emitted into MCP tool names for this runtime mode.
/// Every discovery, catalog, and routing surface uses this function so a
/// sanitized collision cannot become a first-match dispatch ambiguity.
#[must_use]
pub fn runtime_server_prefix(name: &str, sanitize: bool) -> String {
    if sanitize {
        name.replace('-', "_")
    } else {
        name.to_owned()
    }
}

/// Whether a server name can be losslessly separated from its remote tool
/// name under the runtime's `server__tool` wire convention.
#[must_use]
pub fn runtime_server_prefix_is_unambiguous(name: &str, sanitize: bool) -> bool {
    !runtime_server_prefix(name, sanitize).contains("__")
}

/// Dedup a precedence-ordered source list: first valid claimant of an emitted
/// runtime prefix wins. Invalid entries are dropped before they can claim it.
fn dedup_valid_first_wins(
    sources: Vec<McpServerEntry>,
    sanitize_server_names: bool,
) -> Vec<McpServerEntry> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for entry in sources {
        let prefix = runtime_server_prefix(&entry.name, sanitize_server_names);
        if entry.is_valid()
            && runtime_server_prefix_is_unambiguous(&entry.name, sanitize_server_names)
            && seen.insert(prefix)
        {
            out.push(entry);
        }
    }
    out
}

/// Resolve the merged, deduped MCP server list.
///
/// Sources, in precedence order (earlier wins on a name clash):
/// 1. `newt_servers` — newt's own `config.toml` `[[mcp_servers]]`.
/// 2. `~/.newt/mcp.toml` — the newt-owned broken-out source (`newt_mcp_toml`
///    path; pass `None` to skip). Same `[[mcp_servers]]` schema as (1).
/// 3. `~/.claude.json` `mcpServers` — Claude Code user config (`home`; `None`
///    skips it).
/// 4. `<workspace>/.mcp.json` `mcpServers` — Claude Code project config.
///
/// Both newt-owned sources (1, 2) outrank the borrowed Claude overlays. On a
/// name clash the higher-precedence source wins; invalid entries are dropped.
/// Missing/malformed sources are non-fatal.
pub fn discover(
    newt_servers: &[McpServerEntry],
    newt_mcp_toml: Option<&Path>,
    home: Option<&Path>,
    workspace: &Path,
) -> Vec<McpServerEntry> {
    discover_with_namespace_mode(newt_servers, newt_mcp_toml, home, workspace, false)
}

/// Discover with the caller's actual tool-name sanitization mode. When
/// sanitization is enabled, `foo-bar` and `foo_bar` claim the same emitted
/// prefix and only the higher-precedence entry survives. With it disabled the
/// raw names remain distinct.
pub fn discover_with_namespace_mode(
    newt_servers: &[McpServerEntry],
    newt_mcp_toml: Option<&Path>,
    home: Option<&Path>,
    workspace: &Path,
    sanitize_server_names: bool,
) -> Vec<McpServerEntry> {
    // Provenance is stamped at each merge point (the #1301 trust boundary):
    // the two newt-owned sources are TRUSTED, the two borrowed Claude overlays
    // are UNTRUSTED (also enforced at the `parse_claude_mcp` funnel).
    let trusted = |mut e: McpServerEntry| {
        e.trust = McpTrust::Trusted;
        e
    };
    // `newt_servers` come from `Config::resolve`, which already stamps a
    // walked-up project `.newt/config.toml`'s entries UNTRUSTED (a cloned repo
    // can ship one — the residual #1301 vector). PRESERVE that mark; only a
    // genuinely newt-owned entry (default-Trusted, or a hand-built one) is
    // (re-)stamped Trusted. This closure must never re-elevate an Untrusted
    // entry back to Trusted.
    let preserve_or_trust = |mut e: McpServerEntry| {
        if e.trust != McpTrust::Untrusted {
            e.trust = McpTrust::Trusted;
        }
        e
    };
    let untrusted = |mut e: McpServerEntry| {
        e.trust = McpTrust::Untrusted;
        e
    };
    let mut sources: Vec<McpServerEntry> = Vec::new();
    sources.extend(newt_servers.iter().cloned().map(preserve_or_trust));
    if let Some(path) = newt_mcp_toml {
        // `~/.newt/mcp.toml` is a purely user-owned source (never a walk-up),
        // so its entries are unconditionally TRUSTED.
        sources.extend(load_newt_mcp_toml(path).into_iter().map(trusted));
    }
    if let Some(home) = home {
        sources.extend(
            load_claude_file(&home.join(".claude.json"))
                .into_iter()
                .map(untrusted),
        );
    }
    sources.extend(
        load_claude_file(&workspace.join(".mcp.json"))
            .into_iter()
            .map(untrusted),
    );
    dedup_valid_first_wins(sources, sanitize_server_names)
}

#[cfg(test)]
#[path = "mcp_tests/mod.rs"]
mod tests;

// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 15:16 EDT | Date: 2026-08-12
