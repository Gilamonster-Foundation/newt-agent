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

/// One discovered MCP server, in a shape that parses from both Claude Code's
/// `mcpServers` JSON entries and newt's `[[mcp_servers]]` TOML tables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerEntry {
    /// Server name. In newt TOML this is the `name` field; for a Claude entry it
    /// is injected from the `mcpServers` map key (see [`parse_claude_mcp`]).
    #[serde(default)]
    pub name: String,

    /// Transport. Defaults to [`TransportKind::Stdio`] when absent.
    #[serde(default, rename = "type")]
    pub transport: TransportKind,

    /// stdio: the executable to spawn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// stdio: arguments to the executable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// stdio: extra environment for the child.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,

    /// sse/http: the endpoint URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// sse/http: extra request headers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
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

/// Resolve the merged, deduped MCP server list.
///
/// `newt_servers` are newt's own `[[mcp_servers]]` entries (highest precedence).
/// `home` is the user's home directory (for `~/.claude.json`); pass `None` to
/// skip the user-config source. `workspace` is the project root (for
/// `.mcp.json`). On a name clash the higher-precedence source wins; invalid
/// entries are dropped.
pub fn discover(
    newt_servers: &[McpServerEntry],
    home: Option<&Path>,
    workspace: &Path,
) -> Vec<McpServerEntry> {
    let mut sources: Vec<McpServerEntry> = Vec::new();
    sources.extend(newt_servers.iter().cloned());
    if let Some(home) = home {
        sources.extend(load_claude_file(&home.join(".claude.json")));
    }
    sources.extend(load_claude_file(&workspace.join(".mcp.json")));

    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for entry in sources {
        if entry.is_valid() && seen.insert(entry.name.clone()) {
            out.push(entry);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(fs.env.get("ROOT").map(String::as_str), Some("/tmp"));

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
            name: "a".into(),
            transport: TransportKind::Stdio,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
        };
        let sse_no_url = McpServerEntry {
            transport: TransportKind::Sse,
            ..stdio_no_cmd.clone()
        };
        assert!(!stdio_no_cmd.is_valid());
        assert!(!sse_no_url.is_valid());
        // discover drops them.
        let got = discover(&[stdio_no_cmd, sse_no_url], None, Path::new("/nonexistent"));
        assert!(got.is_empty());
    }

    #[test]
    fn newt_entry_wins_on_name_clash_and_dedups() {
        // Two newt entries with the same name -> first wins; a later source with
        // the same name is ignored.
        let newt = vec![
            McpServerEntry {
                name: "dup".into(),
                transport: TransportKind::Stdio,
                command: Some("newt-one".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
            },
            McpServerEntry {
                name: "dup".into(),
                transport: TransportKind::Stdio,
                command: Some("newt-two".into()),
                args: vec![],
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
            },
        ];
        let got = discover(&newt, None, Path::new("/nonexistent"));
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

        let got = discover(&[], Some(&home), &ws);
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
}
