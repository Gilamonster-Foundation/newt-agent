//! Shared MCP server discovery.
//!
//! newt does not define its MCP servers in isolation. It **auto-discovers the
//! same servers you already configured for Claude Code** (so you do not
//! duplicate config) and merges in a newt-native `[[mcp_servers]]` section for
//! extras or overrides. This module only *resolves the merged list*; actually
//! connecting to the servers (the MCP client transport) is a separate layer.
//!
//! Sources, in precedence order (earlier wins on a name clash):
//! 1. newt's own `[[mcp_servers]]` (from `~/.newt/config.toml`)
//! 2. Claude Code user config: `~/.claude.json` → `mcpServers`
//! 3. Project config: `<workspace>/.mcp.json` → `mcpServers`
//!
//! One [`McpServerEntry`] type parses **both** shapes: Claude's `mcpServers`
//! map values and newt's TOML tables have the same fields (`command`/`args`/
//! `env` for stdio; `type` + `url`/`headers` for sse/http), so a single struct
//! serves as the universal target — the only difference is that Claude carries
//! the server name as the map key while newt's TOML carries it as a `name`
//! field.

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

/// Classify the text inside a `${...}` token. Pure. A bare `${NAME}` (no scheme
/// prefix) is an env var; an unknown `scheme:` prefix is a loud error rather
/// than a silently-wrong lookup.
fn classify_token(inner: &str) -> Result<InterpToken> {
    match inner.split_once(':') {
        None => Ok(InterpToken::Env(inner.to_string())),
        Some(("env", rest)) => Ok(InterpToken::Env(rest.to_string())),
        Some(("file", rest)) => Ok(InterpToken::File(rest.to_string())),
        Some(("cmd", rest)) => Ok(InterpToken::Cmd(rest.to_string())),
        Some((scheme, _)) => Err(NewtError::Config(format!(
            "unknown interpolation scheme `{scheme}:` in `${{{inner}}}` \
             (expected `env:`, `file:`, `cmd:`, or a bare `${{VAR}}`)"
        ))),
    }
}

/// The pure core of [`interpolate`]: split `template` into literal runs and
/// `${...}` tokens, resolving each token through the injected `resolve`. Literal
/// text around tokens is preserved verbatim. An unterminated `${` is an error.
/// Kept generic over the resolver so the parsing/reassembly is unit-tested with
/// literals — no env/fs/subprocess.
fn interpolate_with<F>(template: &str, resolve: F) -> Result<String>
where
    F: Fn(&InterpToken) -> Result<String>,
{
    // Fast path: the overwhelmingly common literal (a path, a log level, an
    // org id) carries no token and is returned byte-for-byte.
    if !template.contains("${") {
        return Ok(template.to_string());
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(NewtError::Config(format!(
                "unterminated `${{` in MCP value `{template}`"
            )));
        };
        let token = classify_token(&after[..end])?;
        out.push_str(&resolve(&token)?);
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
/// own trust level — it is config the operator authored, exactly like a line in
/// their shell profile. Resolution happens in newt's own (unconfined) process,
/// just before the child env / HTTP headers are built, and the result is wrapped
/// in [`Secret`]; it is never written back to config and never enters newt's own
/// process env.
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
            Some(parsed)
        })
        .collect()
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

/// Dedup a precedence-ordered source list: first valid claimant of a name wins,
/// invalid entries (a stdio server with no `command`, an sse/http with no `url`)
/// are dropped before they can claim a name. Pure — the merge/precedence rule is
/// unit-tested with in-memory entries.
fn dedup_valid_first_wins(sources: Vec<McpServerEntry>) -> Vec<McpServerEntry> {
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for entry in sources {
        if entry.is_valid() && seen.insert(entry.name.clone()) {
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
    let mut sources: Vec<McpServerEntry> = Vec::new();
    sources.extend(newt_servers.iter().cloned());
    if let Some(path) = newt_mcp_toml {
        sources.extend(load_newt_mcp_toml(path));
    }
    if let Some(home) = home {
        sources.extend(load_claude_file(&home.join(".claude.json")));
    }
    sources.extend(load_claude_file(&workspace.join(".mcp.json")));
    dedup_valid_first_wins(sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_keywords_round_trip() {
        for kind in [
            TransportKind::Stdio,
            TransportKind::Sse,
            TransportKind::Http,
        ] {
            assert_eq!(TransportKind::from_keyword(kind.as_str()), Some(kind));
        }
        assert_eq!(TransportKind::Stdio.as_str(), "stdio");
        assert_eq!(TransportKind::from_keyword("grpc"), None);
    }

    #[test]
    fn parses_claude_stdio_and_sse_entries() {
        let cfg = serde_json::json!({
            "mcpServers": {
                "filesystem": { "command": "npx", "args": ["-y", "@mcp/fs"], "env": { "ROOT": "/tmp" } },
                "remote":     { "type": "sse", "url": "https://mcp.example/sse", "headers": { "Authorization": "Bearer x" } }
            }
        });
        let mut got = parse_claude_mcp(&cfg);
        got.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(got.len(), 2);

        let fs = got.iter().find(|e| e.name == "filesystem").unwrap();
        assert_eq!(fs.transport, TransportKind::Stdio); // inferred (no "type")
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.args, vec!["-y", "@mcp/fs"]);
        assert_eq!(
            fs.env.get("ROOT").and_then(SecretValue::as_literal),
            Some("/tmp")
        );

        let remote = got.iter().find(|e| e.name == "remote").unwrap();
        assert_eq!(remote.transport, TransportKind::Sse);
        assert_eq!(remote.url.as_deref(), Some("https://mcp.example/sse"));
    }

    #[test]
    fn missing_mcpservers_key_is_empty_not_error() {
        assert!(parse_claude_mcp(&serde_json::json!({ "other": 1 })).is_empty());
    }

    #[test]
    fn invalid_entries_are_dropped() {
        // stdio with no command, and sse with no url — both invalid.
        let stdio_no_cmd = McpServerEntry {
            enabled: true,
            name: "a".into(),
            transport: TransportKind::Stdio,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            request_timeout_secs: None,
        };
        let sse_no_url = McpServerEntry {
            transport: TransportKind::Sse,
            ..stdio_no_cmd.clone()
        };
        assert!(!stdio_no_cmd.is_valid());
        assert!(!sse_no_url.is_valid());
        // discover drops them.
        let got = discover(
            &[stdio_no_cmd, sse_no_url],
            None,
            None,
            Path::new("/nonexistent"),
        );
        assert!(got.is_empty());
    }

    #[test]
    fn newt_entry_wins_on_name_clash_and_dedups() {
        // Two newt entries with the same name -> first wins; a later source with
        // the same name is ignored.
        let newt = vec![
            McpServerEntry {
                enabled: true,
                name: "dup".into(),
                transport: TransportKind::Stdio,
                command: Some("newt-one".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                request_timeout_secs: None,
            },
            McpServerEntry {
                enabled: true,
                name: "dup".into(),
                transport: TransportKind::Stdio,
                command: Some("newt-two".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                request_timeout_secs: None,
            },
        ];
        let got = discover(&newt, None, None, Path::new("/nonexistent"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].command.as_deref(), Some("newt-one"));
    }

    #[test]
    fn discovers_from_claude_user_and_project_files() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(
            home.join(".claude.json"),
            r#"{ "mcpServers": { "user_srv": { "command": "u" } } }"#,
        )
        .unwrap();
        std::fs::write(
            ws.join(".mcp.json"),
            r#"{ "mcpServers": { "proj_srv": { "command": "p" } } }"#,
        )
        .unwrap();

        let got = discover(&[], None, Some(&home), &ws);
        let names: std::collections::BTreeSet<_> = got.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains("user_srv"),
            "user config discovered: {names:?}"
        );
        assert!(
            names.contains("proj_srv"),
            "project config discovered: {names:?}"
        );
    }

    // ---- SecretValue: deserialization + resolution ----

    #[derive(Deserialize)]
    struct Holder {
        v: SecretValue,
    }

    #[test]
    fn secret_value_string_deserializes_to_literal() {
        // TOML (newt config) and JSON (Claude import) both map a bare string to
        // a Literal — backward-compatible with every existing config.
        let toml_h: Holder = toml::from_str(r#"v = "hello""#).unwrap();
        assert_eq!(toml_h.v, SecretValue::Literal("hello".into()));
        let json_v: SecretValue = serde_json::from_value(serde_json::json!("hi")).unwrap();
        assert_eq!(json_v, SecretValue::Literal("hi".into()));
    }

    #[test]
    fn secret_value_table_deserializes_to_ref() {
        let cmd_h: Holder =
            toml::from_str(r#"v = { cmd = "vault kv get -field=token secret/gh" }"#).unwrap();
        assert_eq!(
            cmd_h.v,
            SecretValue::Ref(SecretRef {
                cmd: Some("vault kv get -field=token secret/gh".into()),
                ..Default::default()
            })
        );
        let env_h: Holder = toml::from_str(r#"v = { env = "TOK" }"#).unwrap();
        assert!(matches!(
            env_h.v,
            SecretValue::Ref(SecretRef { env: Some(_), .. })
        ));
        let file_h: Holder = toml::from_str(r#"v = { file = "~/.secrets/x" }"#).unwrap();
        assert!(matches!(
            file_h.v,
            SecretValue::Ref(SecretRef { file: Some(_), .. })
        ));
    }

    #[test]
    fn secret_value_literal_resolves_verbatim_without_tokens() {
        // No `${...}` → no env/fs/subprocess touched: a pure pass-through.
        assert_eq!(
            SecretValue::literal("info").resolve().unwrap().expose(),
            "info"
        );
        assert_eq!(SecretValue::literal("").resolve().unwrap().expose(), "");
    }

    #[test]
    fn secret_value_as_literal_and_roundtrips_through_toml() {
        assert_eq!(SecretValue::literal("x").as_literal(), Some("x"));
        assert_eq!(SecretValue::Ref(SecretRef::default()).as_literal(), None);
        // A Literal serializes as a bare string; a Ref as an inline table.
        #[derive(Serialize)]
        struct H {
            v: SecretValue,
        }
        let lit = toml::to_string(&H {
            v: SecretValue::literal("hi"),
        })
        .unwrap();
        assert!(lit.contains("v = \"hi\""), "{lit}");
        let refd = toml::to_string(&H {
            v: SecretValue::Ref(SecretRef {
                cmd: Some("vault kv get x".into()),
                ..Default::default()
            }),
        })
        .unwrap();
        assert!(refd.contains("cmd"), "{refd}");
        assert!(refd.contains("vault kv get x"), "{refd}");
    }

    // ---- ${...} interpolation (pure: injected resolver) ----

    #[test]
    fn interpolate_returns_a_token_free_literal_verbatim() {
        // The resolver must never be called for a token-free literal.
        let out = interpolate_with("Bearer plain /tmp org-12345", |_| {
            panic!("no token should be resolved")
        })
        .unwrap();
        assert_eq!(out, "Bearer plain /tmp org-12345");
    }

    #[test]
    fn interpolate_embeds_a_token_in_surrounding_literal() {
        let out = interpolate_with(
            "Bearer ${cmd:vault kv get -field=token secret/data/github}!",
            |t| {
                assert_eq!(
                    t,
                    &InterpToken::Cmd("vault kv get -field=token secret/data/github".into())
                );
                Ok("RESOLVED".to_string())
            },
        )
        .unwrap();
        assert_eq!(out, "Bearer RESOLVED!");
    }

    #[test]
    fn interpolate_handles_multiple_tokens_of_every_scheme() {
        let out = interpolate_with("${env:A}/${file:/p}/${X}", |t| {
            Ok(match t {
                InterpToken::Env(v) => format!("E<{v}>"),
                InterpToken::File(p) => format!("F<{p}>"),
                InterpToken::Cmd(c) => format!("C<{c}>"),
            })
        })
        .unwrap();
        // `${X}` (bare, no scheme) resolves as an env var.
        assert_eq!(out, "E<A>/F</p>/E<X>");
    }

    #[test]
    fn classify_token_maps_schemes_and_rejects_unknown() {
        assert_eq!(
            classify_token("VAR").unwrap(),
            InterpToken::Env("VAR".into())
        );
        assert_eq!(
            classify_token("env:VAR").unwrap(),
            InterpToken::Env("VAR".into())
        );
        assert_eq!(
            classify_token("file:~/.secrets/x").unwrap(),
            InterpToken::File("~/.secrets/x".into())
        );
        assert_eq!(
            classify_token("cmd:vault kv get -field=token secret/x").unwrap(),
            InterpToken::Cmd("vault kv get -field=token secret/x".into())
        );
        // Unknown scheme is a loud error, not a silently-wrong lookup.
        assert!(classify_token("bogus:thing").is_err());
    }

    #[test]
    fn interpolate_missing_reference_fails_loudly_not_empty() {
        // A token that the resolver can't satisfy propagates as an error — a
        // missing env var must fail the spawn, never become a silent empty.
        let err = interpolate_with("${env:MISSING}", |_| {
            Err(NewtError::Config("environment variable not set".into()))
        })
        .unwrap_err();
        assert!(format!("{err}").contains("not set"));
    }

    #[test]
    fn interpolate_unterminated_token_is_an_error() {
        assert!(interpolate_with("Bearer ${cmd:oops", |_| Ok("x".into())).is_err());
    }

    // ---- ~/.newt/mcp.toml source: parse + precedence ----

    fn stdio(name: &str, command: &str) -> McpServerEntry {
        McpServerEntry {
            name: name.into(),
            enabled: true,
            transport: TransportKind::Stdio,
            command: Some(command.into()),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            request_timeout_secs: None,
        }
    }

    #[test]
    fn parse_newt_mcp_toml_reads_servers_and_tolerates_garbage() {
        let text = r#"
[[mcp_servers]]
name = "a"
command = "a-mcp"

[[mcp_servers]]
name = "b"
type = "http"
url = "https://x/mcp"
"#;
        let got = parse_newt_mcp_toml(text);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "a");
        assert_eq!(got[0].command.as_deref(), Some("a-mcp"));
        assert_eq!(got[1].transport, TransportKind::Http);
        // Malformed TOML → empty (non-fatal), missing section → empty.
        assert!(parse_newt_mcp_toml("not = = toml [").is_empty());
        assert!(parse_newt_mcp_toml("other = 1").is_empty());
    }

    #[test]
    fn parse_newt_mcp_toml_reads_secret_refs_and_literals() {
        let text = r#"
[[mcp_servers]]
name = "gh"
command = "gh-mcp"
[mcp_servers.env]
GH_TOKEN = { cmd = "vault kv get -field=token secret/gh" }
RUST_LOG = "debug"
"#;
        let got = parse_newt_mcp_toml(text);
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].env.get("RUST_LOG"),
            Some(&SecretValue::literal("debug"))
        );
        assert!(matches!(
            got[0].env.get("GH_TOKEN"),
            Some(SecretValue::Ref(SecretRef { cmd: Some(_), .. }))
        ));
    }

    #[test]
    fn discover_ranks_config_over_mcp_toml_over_claude() {
        // Pure precedence over in-memory sources: config.toml newt entry wins,
        // then ~/.newt/mcp.toml, then the Claude overlays. First-name-wins,
        // order preserved.
        let merged = dedup_valid_first_wins(vec![
            stdio("dup", "config-wins"),
            stdio("mcp-only", "m"),
            stdio("dup", "mcp-toml-loses"),
            stdio("claude-only", "c"),
            stdio("dup", "claude-loses"),
        ]);
        let names: Vec<&str> = merged.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["dup", "mcp-only", "claude-only"]);
        assert_eq!(merged[0].command.as_deref(), Some("config-wins"));
    }

    #[test]
    fn discover_reads_mcp_toml_as_a_newt_owned_source() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_toml = dir.path().join("mcp.toml");
        std::fs::write(
            &mcp_toml,
            "[[mcp_servers]]\nname = \"broken-out\"\ncommand = \"bo-mcp\"\n",
        )
        .unwrap();
        let got = discover(&[], Some(&mcp_toml), None, Path::new("/nonexistent"));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "broken-out");
        // A missing mcp.toml path is simply skipped (non-fatal).
        assert!(discover(
            &[],
            Some(Path::new("/no/such/mcp.toml")),
            None,
            Path::new("/nope")
        )
        .is_empty());
    }
}
