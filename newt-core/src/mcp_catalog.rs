//! Curated MCP server catalog (`newt mcp install <name>`).
//!
//! Three-Cs: the catalog is pure DATA, not code — the language-pack model
//! (`api_surface.rs`) applied to MCP servers. A bundled default ships inside
//! the binary (`mcp_catalog/default.toml`); a user drop-in
//! (`~/.newt/mcp-catalog.toml`) and a project drop-in
//! (`.newt/mcp-catalog.toml`) layer over it, merged **by name** with later
//! layers winning — so publishing a new installable server is config, not
//! code. This module is fully pure (parse + merge); the CLI owns the file
//! reads.

use serde::Deserialize;

use crate::error::{NewtError, Result};
use crate::mcp::McpServerEntry;

const BUNDLED_CATALOG: &str = include_str!("mcp_catalog/default.toml");

/// One installable catalog entry: a human-facing `description` plus the
/// [`McpServerEntry`] fields (flattened — `command`, `args`, `type`, `url`, …)
/// that `newt mcp install` writes into `[[mcp_servers]]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct McpCatalogEntry {
    /// Catalog (and installed-server) name.
    pub name: String,
    /// Short human-facing description, shown on install and in error listings.
    #[serde(default)]
    pub description: String,
    // Deserialize-only: the outer `name` consumes the TOML key, so a
    // round-trip serialize would emit `name` twice. Nothing serializes a
    // catalog — installs go through `Config::with_mcp_server_added`.
    #[serde(flatten)]
    server: McpServerEntry,
}

impl McpCatalogEntry {
    /// The server registration this entry installs, with `name` filled in
    /// (the flattened body never carries it — the catalog key does).
    #[must_use]
    pub fn server(&self) -> McpServerEntry {
        let mut server = self.server.clone();
        server.name = self.name.clone();
        server
    }
}

/// On-disk catalog shape: a `[[servers]]` list.
#[derive(Debug, Default, Deserialize)]
struct McpCatalog {
    #[serde(default)]
    servers: Vec<McpCatalogEntry>,
}

/// Parse a catalog document. Strict: a malformed catalog is a loud error, not
/// an empty list — `newt mcp install` acting on a half-read catalog would be
/// worse than failing. Missing `[[servers]]` is an empty catalog, not an error.
pub fn parse_catalog(text: &str) -> Result<Vec<McpCatalogEntry>> {
    let catalog: McpCatalog = toml::from_str(text)
        .map_err(|e| NewtError::Config(format!("MCP catalog is not valid TOML: {e}")))?;
    Ok(catalog.servers)
}

/// Merge catalog layers **by name** — later layers win (bundled < user
/// `~/.newt/mcp-catalog.toml` < project `.newt/mcp-catalog.toml`), keeping
/// first-seen order. The same contract as `api_surface::merge_packs`.
#[must_use]
pub fn merge_catalogs(layers: Vec<Vec<McpCatalogEntry>>) -> Vec<McpCatalogEntry> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: std::collections::HashMap<String, McpCatalogEntry> =
        std::collections::HashMap::new();
    for layer in layers {
        for entry in layer {
            if !by_name.contains_key(&entry.name) {
                order.push(entry.name.clone());
            }
            by_name.insert(entry.name.clone(), entry);
        }
    }
    order
        .into_iter()
        .filter_map(|n| by_name.remove(&n))
        .collect()
}

/// The catalog bundled into the binary. Guarded by a unit test, so the
/// `expect` cannot fire on a shipped build.
#[must_use]
pub fn builtin_catalog() -> Vec<McpCatalogEntry> {
    parse_catalog(BUNDLED_CATALOG).expect("bundled MCP catalog must parse")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::TransportKind;

    #[test]
    fn bundled_catalog_parses_and_ships_scrybe() {
        let catalog = builtin_catalog();
        let scrybe = catalog
            .iter()
            .find(|e| e.name == "scrybe")
            .expect("scrybe ships in the bundled catalog");
        assert_eq!(
            scrybe.description,
            "Scrybe Markdown editor — document tools over MCP"
        );
        let server = scrybe.server();
        assert_eq!(server.name, "scrybe");
        assert_eq!(server.command.as_deref(), Some("scrybe-mcp-server"));
        assert_eq!(server.args, vec!["stdio"]);
        assert_eq!(server.transport, TransportKind::Stdio);
        assert!(server.enabled);
        assert!(server.is_valid(), "bundled entries must be installable");
    }

    #[test]
    fn parse_catalog_reads_flattened_server_fields() {
        let text = r#"
[[servers]]
name = "remote"
description = "an sse example"
type = "sse"
url = "https://mcp.example/sse"
headers = { Authorization = "Bearer x" }
request_timeout_secs = 90

[[servers]]
name = "fs"
command = "mcp-fs"
env = { ROOT = "/tmp" }
"#;
        let got = parse_catalog(text).unwrap();
        assert_eq!(got.len(), 2);
        let remote = got[0].server();
        assert_eq!(remote.transport, TransportKind::Sse);
        assert_eq!(remote.url.as_deref(), Some("https://mcp.example/sse"));
        assert_eq!(
            remote.headers.get("Authorization").map(String::as_str),
            Some("Bearer x")
        );
        assert_eq!(remote.request_timeout_secs, Some(90));
        let fs = got[1].server();
        assert_eq!(fs.command.as_deref(), Some("mcp-fs"));
        assert_eq!(fs.env.get("ROOT").map(String::as_str), Some("/tmp"));
        assert_eq!(got[1].description, "", "description is optional");
    }

    #[test]
    fn parse_catalog_is_strict_but_tolerates_an_empty_document() {
        assert!(parse_catalog("not toml [").is_err());
        assert!(parse_catalog("").unwrap().is_empty());
    }

    #[test]
    fn merge_catalogs_later_layer_wins_by_name_keeping_order() {
        let base = parse_catalog(
            "[[servers]]\nname = \"a\"\ncommand = \"a-v1\"\n\
             [[servers]]\nname = \"b\"\ncommand = \"b-v1\"\n",
        )
        .unwrap();
        let overlay = parse_catalog(
            "[[servers]]\nname = \"b\"\ncommand = \"b-v2\"\n\
             [[servers]]\nname = \"c\"\ncommand = \"c-v1\"\n",
        )
        .unwrap();
        let merged = merge_catalogs(vec![base, overlay]);
        let names: Vec<&str> = merged.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"], "first-seen order kept");
        assert_eq!(
            merged[1].server().command.as_deref(),
            Some("b-v2"),
            "later layer wins on a name clash"
        );
    }
}
