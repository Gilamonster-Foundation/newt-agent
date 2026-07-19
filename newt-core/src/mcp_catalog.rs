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

/// On-disk catalog shape: a `[[servers]]` list. `deny_unknown_fields` keeps
/// the strict contract honest: a typo'd section (`[[server]]`, `[[Servers]]`)
/// must be a loud parse error, never a silently empty catalog.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
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
    merge_catalog_layers(layers.into_iter().map(|layer| ((), layer)).collect())
        .into_iter()
        .map(|(entry, ())| entry)
        .collect()
}

/// [`merge_catalogs`] with per-layer provenance: each layer carries a tag
/// (e.g. "bundled" / the drop-in path) and every merged entry keeps the tag
/// of the layer that won it — so an error about a broken entry can name the
/// file to fix. Pure.
#[must_use]
pub fn merge_catalog_layers<T: Clone>(
    layers: Vec<(T, Vec<McpCatalogEntry>)>,
) -> Vec<(McpCatalogEntry, T)> {
    let mut order: Vec<String> = Vec::new();
    let mut by_name: std::collections::HashMap<String, (McpCatalogEntry, T)> =
        std::collections::HashMap::new();
    for (tag, layer) in layers {
        for entry in layer {
            if !by_name.contains_key(&entry.name) {
                order.push(entry.name.clone());
            }
            by_name.insert(entry.name.clone(), (entry, tag.clone()));
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

/// Upsert a `[[servers]]` catalog entry into catalog TOML `text`, preserving
/// comments and formatting (`newt mcp probe --to-catalog`, #1292). PURE (no
/// I/O) like `Config::with_mcp_server_added`; the caller does the read/write.
/// A same-name entry is **replaced in place** — the catalog merge is
/// later-wins-by-name, so a probe re-run refreshes the entry rather than
/// erroring — and other entries are untouched. Rejects an unnamed or
/// uninstallable server (`McpServerEntry::is_valid`).
pub fn with_catalog_entry(
    text: &str,
    description: &str,
    server: &McpServerEntry,
) -> Result<String> {
    crate::mcp::validate_entry_for_write(server)?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| NewtError::Config(format!("MCP catalog is not valid TOML: {e}")))?;
    let servers = doc
        .as_table_mut()
        .entry("servers")
        .or_insert(toml_edit::Item::ArrayOfTables(
            toml_edit::ArrayOfTables::new(),
        ));
    let arr = servers
        .as_array_of_tables_mut()
        .ok_or_else(|| NewtError::Config("[[servers]] is not an array of tables".to_string()))?;
    // An absent description stays an absent key — never `description = ""`.
    let table =
        crate::mcp::entry_to_toml_table(server, Some(description).filter(|d| !d.is_empty()))?;
    let existing = arr
        .iter_mut()
        .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(server.name.as_str()));
    match existing {
        Some(slot) => {
            // The table's decor carries the comments ABOVE its [[servers]]
            // header (e.g. the file banner when replacing the first entry) —
            // keep it across the swap; only the entry's fields are replaced.
            let decor = slot.decor().clone();
            *slot = table;
            *slot.decor_mut() = decor;
        }
        None => arr.push(table),
    }
    Ok(doc.to_string())
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
            remote
                .headers
                .get("Authorization")
                .and_then(crate::mcp::SecretValue::as_literal),
            Some("Bearer x")
        );
        assert_eq!(remote.request_timeout_secs, Some(90));
        let fs = got[1].server();
        assert_eq!(fs.command.as_deref(), Some("mcp-fs"));
        assert_eq!(
            fs.env
                .get("ROOT")
                .and_then(crate::mcp::SecretValue::as_literal),
            Some("/tmp")
        );
        assert_eq!(got[1].description, "", "description is optional");
    }

    #[test]
    fn parse_catalog_is_strict_but_tolerates_an_empty_document() {
        assert!(parse_catalog("not toml [").is_err());
        assert!(parse_catalog("").unwrap().is_empty());
    }

    #[test]
    fn parse_catalog_rejects_unknown_top_level_sections() {
        // A typo'd section must be a loud error, never an empty catalog —
        // the "no half-read catalog" contract.
        assert!(parse_catalog("[[server]]\nname = \"x\"\ncommand = \"y\"\n").is_err());
        assert!(parse_catalog("[[Servers]]\nname = \"x\"\ncommand = \"y\"\n").is_err());
    }

    fn stdio_server(name: &str, command: &str) -> crate::mcp::McpServerEntry {
        crate::mcp::McpServerEntry {
            name: name.into(),
            enabled: true,
            transport: TransportKind::Stdio,
            command: Some(command.into()),
            args: vec!["stdio".into()],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            request_timeout_secs: None,
        }
    }

    #[test]
    fn with_catalog_entry_appends_preserving_comments() {
        let text = "# curated\n[[servers]]\nname = \"keep\"\ncommand = \"keep-mcp\" # note\n";
        let out = with_catalog_entry(
            text,
            "Scrybe Markdown editor — document tools over MCP",
            &stdio_server("scrybe", "scrybe-mcp-server"),
        )
        .unwrap();
        assert!(out.contains("# curated"), "comment lost: {out}");
        assert!(out.contains("# note"), "inline comment lost: {out}");
        let parsed = parse_catalog(&out).unwrap();
        assert_eq!(parsed.len(), 2);
        let scrybe = parsed.iter().find(|e| e.name == "scrybe").unwrap();
        assert_eq!(
            scrybe.description,
            "Scrybe Markdown editor — document tools over MCP"
        );
        let server = scrybe.server();
        assert_eq!(server.command.as_deref(), Some("scrybe-mcp-server"));
        assert_eq!(server.args, vec!["stdio"]);
    }

    #[test]
    fn with_catalog_entry_replaces_a_same_name_entry_in_place() {
        let text = "[[servers]]\nname = \"a\"\ncommand = \"a-v1\"\n\
                    [[servers]]\nname = \"b\"\ncommand = \"b-v1\"\n";
        let out = with_catalog_entry(text, "refreshed", &stdio_server("a", "a-v2")).unwrap();
        let parsed = parse_catalog(&out).unwrap();
        let names: Vec<&str> = parsed.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"], "position kept, no duplicate added");
        assert_eq!(parsed[0].server().command.as_deref(), Some("a-v2"));
        assert_eq!(parsed[0].description, "refreshed");
        assert_eq!(out.matches("name = \"a\"").count(), 1);
        assert!(out.contains("b-v1"), "unrelated entry untouched");
    }

    #[test]
    fn with_catalog_entry_replace_keeps_the_banner_and_other_comments() {
        // The file banner is prefix decor on the FIRST [[servers]] header —
        // a naive slot assignment drops it when that entry is replaced.
        let text = "\
# Curated catalog — hands off

[[servers]]
name = \"a\"
command = \"a-v1\"

[[servers]]
name = \"b\"
command = \"b-v1\" # keep b note
";
        let out = with_catalog_entry(text, "refreshed", &stdio_server("a", "a-v2")).unwrap();
        assert!(
            out.contains("# Curated catalog — hands off"),
            "banner lost on replace: {out}"
        );
        assert!(
            out.contains("# keep b note"),
            "unrelated inline comment lost: {out}"
        );
        let parsed = parse_catalog(&out).unwrap();
        assert_eq!(parsed[0].server().command.as_deref(), Some("a-v2"));
        assert_eq!(parsed[1].server().command.as_deref(), Some("b-v1"));
    }

    #[test]
    fn with_catalog_entry_omits_an_empty_description() {
        // A probe of a server with no title/instructions must not write
        // `description = ""` — absent key, not empty string.
        let out = with_catalog_entry("", "", &stdio_server("fs", "mcp-fs")).unwrap();
        assert!(!out.contains("description"), "{out}");
        let parsed = parse_catalog(&out).unwrap();
        assert_eq!(parsed[0].description, "");
    }

    #[test]
    fn with_catalog_entry_creates_the_section_in_empty_text() {
        let out = with_catalog_entry("", "desc", &stdio_server("fs", "mcp-fs")).unwrap();
        let parsed = parse_catalog(&out).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "fs");
    }

    #[test]
    fn with_catalog_entry_rejects_uninstallable_and_unnamed_servers() {
        let mut broken = stdio_server("half", "x");
        broken.command = None;
        let err = with_catalog_entry("", "d", &broken).unwrap_err();
        assert!(err.to_string().contains("command"), "{err}");
        let unnamed = crate::mcp::McpServerEntry {
            name: "  ".into(),
            ..stdio_server("x", "x-mcp")
        };
        assert!(with_catalog_entry("", "d", &unnamed).is_err());
    }

    #[test]
    fn merge_catalog_layers_carries_the_winning_origin() {
        let base = parse_catalog(
            "[[servers]]\nname = \"a\"\ncommand = \"a-v1\"\n\
             [[servers]]\nname = \"b\"\ncommand = \"b-v1\"\n",
        )
        .unwrap();
        let overlay = parse_catalog("[[servers]]\nname = \"b\"\ncommand = \"b-v2\"\n").unwrap();
        let merged = merge_catalog_layers(vec![("bundled", base), ("user", overlay)]);
        let tagged: Vec<(&str, &str)> = merged
            .iter()
            .map(|(e, tag)| (e.name.as_str(), *tag))
            .collect();
        assert_eq!(tagged, vec![("a", "bundled"), ("b", "user")]);
        assert_eq!(
            merged[1].0.server().command.as_deref(),
            Some("b-v2"),
            "the origin follows the winning entry"
        );
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
