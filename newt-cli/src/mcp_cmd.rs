//! `newt mcp add|remove|list|install` — manage `[[mcp_servers]]`
//! registrations without hand-editing TOML.
//!
//! Bare `newt mcp` (no subcommand) still runs newt-as-an-MCP-server over
//! stdio — the dispatch in `lib.rs` routes `cmd: None` to `run_mcp` exactly
//! as before. These verbs write through newt-core's pure, comment-preserving
//! writers ([`Config::with_mcp_server_added`] / [`Config::with_mcp_server_removed`]);
//! the only filesystem work here is the read/write of the target file.
//!
//! Write target: the same file `Config::resolve()` reads as its base
//! (`--config` flag > `$NEWT_CONFIG` > `./newt.toml` if present > the user
//! config honoring `NEWT_CONFIG_DIR`) so a write is always visible to the
//! readers; `--project` targets the nearest ancestor `.newt/config.toml`,
//! creating one under the current directory only when no ancestor has one.
//! See [`write_target`].
//!
//! `list` is the *merged discovery view* — newt's own `[[mcp_servers]]` plus
//! the Claude Code overlays (`~/.claude.json`, `./.mcp.json`) that
//! [`newt_core::mcp::discover`] folds in at session start — with each row
//! attributed to its source. Unlike `discover`, an entry that can never
//! connect (a stdio server with no `command`) is *shown* and flagged, not
//! silently dropped: this is the management surface where you'd fix it.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use clap::Subcommand;
use newt_core::mcp::{McpServerEntry, TransportKind};
use newt_core::mcp_catalog::{
    builtin_catalog, merge_catalog_layers, parse_catalog, McpCatalogEntry,
};
use newt_core::Config;

#[derive(Subcommand, Debug)]
pub enum McpCmd {
    /// Register an MCP server in the config (comment-preserving append).
    Add {
        /// Server name (must be unique in the target config).
        name: String,
        /// stdio: the executable to spawn.
        #[arg(long)]
        command: Option<String>,
        /// stdio: one argument for the executable (repeat per argument).
        /// Leading dashes are fine: `--arg --verbose` passes `--verbose` through.
        #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
        args: Vec<String>,
        /// Transport: stdio | sse | http.
        #[arg(long, default_value = "stdio", value_parser = parse_transport)]
        transport: TransportKind,
        /// sse/http: the endpoint URL.
        #[arg(long)]
        url: Option<String>,
        /// stdio: extra environment for the child process (K=V, repeatable).
        #[arg(long = "env", value_name = "K=V")]
        env: Vec<String>,
        /// Per-request timeout override, in seconds.
        #[arg(long, value_name = "N")]
        timeout_secs: Option<u64>,
        /// Write to the project config: the nearest ancestor
        /// `.newt/config.toml`, created under the current directory when no
        /// ancestor has one.
        #[arg(long)]
        project: bool,
    },
    /// Remove a registered MCP server from the config.
    Remove {
        /// Server name to remove.
        name: String,
        /// Target the project config instead of the user config.
        #[arg(long)]
        project: bool,
    },
    /// List the merged discovery view: newt config + Claude Code overlays.
    List,
    /// Probe a candidate MCP server: spawn a command (confined, consented)
    /// or dial an http(s) URL, initialize, list tools, and derive its
    /// registration. Verify-and-enrich only — the target is always explicit.
    Probe(crate::mcp_probe_cmd::ProbeArgs),
    /// Install a server from the curated catalog (bundled + drop-in overlays).
    Install {
        /// Catalog entry name (e.g. `scrybe`).
        name: String,
        /// Write to the project config instead of the user config.
        #[arg(long)]
        project: bool,
    },
}

/// clap `value_parser` for `--transport` (keeps newt-core clap-free — the
/// `ColorMode` pattern).
fn parse_transport(s: &str) -> Result<TransportKind, String> {
    TransportKind::from_keyword(s)
        .ok_or_else(|| format!("invalid transport '{s}' (expected one of: stdio, sse, http)"))
}

/// Entry point dispatched from `newt mcp <subcommand>`.
pub async fn run(cmd: McpCmd, config_path: Option<&Path>) -> anyhow::Result<()> {
    let mut out = std::io::stdout();
    match cmd {
        McpCmd::Add {
            name,
            command,
            args,
            transport,
            url,
            env,
            timeout_secs,
            project,
        } => {
            let entry = build_entry(name, command, args, transport, url, &env, timeout_secs)?;
            let path = add_to_config(&entry, config_path, project)?;
            writeln!(
                out,
                "Registered MCP server '{}' in {}",
                entry.name,
                path.display()
            )?;
            print_next_steps(&mut out)
        }
        McpCmd::Remove { name, project } => {
            let path = write_target(config_path, project)?;
            let text = read_config_text(&path)?;
            let updated = Config::with_mcp_server_removed(&text, &name)?;
            std::fs::write(&path, updated)
                .with_context(|| format!("writing {}", path.display()))?;
            writeln!(out, "Removed MCP server '{name}' from {}", path.display())?;
            Ok(())
        }
        McpCmd::List => cmd_list(config_path, &mut out),
        McpCmd::Probe(probe) => crate::mcp_probe_cmd::run(probe, config_path).await,
        McpCmd::Install { name, project } => {
            let catalog = resolve_catalog()?;
            let Some((chosen, origin)) = catalog.iter().find(|(e, _)| e.name == name) else {
                let names: Vec<&str> = catalog.iter().map(|(e, _)| e.name.as_str()).collect();
                bail!(
                    "no `{name}` in the MCP catalog (available: {})",
                    names.join(", ")
                );
            };
            let server = installable_server(chosen, origin)?;
            let path = add_to_config(&server, config_path, project)?;
            writeln!(
                out,
                "Installed MCP server '{}' ({}) in {}",
                chosen.name,
                chosen.description,
                path.display()
            )?;
            print_next_steps(&mut out)
        }
    }
}

/// The post-add pointer to the verification surfaces.
pub(crate) fn print_next_steps(out: &mut dyn Write) -> anyhow::Result<()> {
    writeln!(
        out,
        "Verify with `newt doctor`; in the TUI, `/mcp` shows live status."
    )?;
    Ok(())
}

/// Resolve the config file a write-verb targets — the SAME file the reader
/// ([`Config::resolve`]) will consult as its base, so an add is always
/// visible to `newt mcp list` / `newt doctor` afterwards:
/// - `--project`: the nearest ancestor `.newt/config.toml` (the walk-up
///   [`Config::project_config_path`]); `cwd/.newt/config.toml` only when NO
///   ancestor has one — never forking a nested config that would shadow the
///   repo root's from a subtree;
/// - else the global `--config` flag;
/// - else `$NEWT_CONFIG`;
/// - else `./newt.toml` when it exists (resolve()'s next base candidate);
/// - else the user config (`$NEWT_CONFIG_DIR/config.toml` / `~/.newt/…`).
pub(crate) fn write_target(config_path: Option<&Path>, project: bool) -> anyhow::Result<PathBuf> {
    if project {
        if let Some(existing) = Config::project_config_path() {
            return Ok(existing);
        }
        return Ok(std::env::current_dir()
            .context("cannot resolve the current directory")?
            .join(".newt")
            .join("config.toml"));
    }
    if let Some(explicit) = config_path {
        return Ok(explicit.to_path_buf());
    }
    if let Some(env_cfg) = std::env::var_os("NEWT_CONFIG").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(env_cfg));
    }
    let local = std::env::current_dir()
        .context("cannot resolve the current directory")?
        .join("newt.toml");
    if local.is_file() {
        return Ok(local);
    }
    Config::user_config_path().ok_or_else(|| anyhow!("cannot resolve ~/.newt (no home dir)"))
}

/// Read the write-target's current text. Only a MISSING file maps to empty
/// text (the first-write case); any other read failure — permissions, a
/// non-UTF-8 byte — aborts loudly. Treating those as empty would rewrite the
/// user's whole config as just the appended entry: silent data loss.
/// Read an OPTIONAL drop-in file: `Ok(None)` when it does not exist, its text
/// when it does, and a loud error naming the path for any other read failure
/// (permissions, a non-UTF-8 byte). Silently skipping a present-but-unreadable
/// drop-in would act on config the operator believes they have overridden —
/// the same read-safety contract as [`read_config_text`].
pub(crate) fn read_optional(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e))
            .with_context(|| format!("cannot read drop-in {}", path.display())),
    }
}

pub(crate) fn read_config_text(path: &Path) -> anyhow::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(anyhow::Error::new(e)).with_context(|| {
            format!(
                "cannot read {} (refusing to rewrite a config that cannot be read back)",
                path.display()
            )
        }),
    }
}

/// Shared add/install write path: read the target (missing file = empty),
/// append through the pure writer, create parent dirs, write back.
pub(crate) fn add_to_config(
    entry: &McpServerEntry,
    config_path: Option<&Path>,
    project: bool,
) -> anyhow::Result<PathBuf> {
    let path = write_target(config_path, project)?;
    let text = read_config_text(&path)?;
    let updated = Config::with_mcp_server_added(&text, entry)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Assemble a validated [`McpServerEntry`] from the `add` flags. Pure.
/// Errors name the flag to fix, not just the field.
fn build_entry(
    name: String,
    command: Option<String>,
    args: Vec<String>,
    transport: TransportKind,
    url: Option<String>,
    env: &[String],
    timeout_secs: Option<u64>,
) -> anyhow::Result<McpServerEntry> {
    match transport {
        TransportKind::Stdio => {
            if command.is_none() {
                bail!("--transport stdio requires --command <CMD>");
            }
            if url.is_some() {
                bail!("--url only applies to --transport sse|http");
            }
        }
        TransportKind::Sse | TransportKind::Http => {
            if url.is_none() {
                bail!("--transport {} requires --url <URL>", transport.as_str());
            }
            if command.is_some() || !args.is_empty() {
                bail!(
                    "--command/--arg only apply to --transport stdio, not {}",
                    transport.as_str()
                );
            }
        }
    }
    Ok(McpServerEntry {
        name,
        enabled: true,
        transport,
        command,
        args,
        env: parse_env_pairs(env)?,
        url,
        headers: BTreeMap::new(),
        request_timeout_secs: timeout_secs,
    })
}

/// Parse repeated `--env K=V` flags. Pure. Rejects a pair with no `=` or an
/// empty key; the value may be empty (explicitly unsetting is legitimate).
pub(crate) fn parse_env_pairs(pairs: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=').filter(|(k, _)| !k.is_empty()) else {
            bail!("invalid --env '{pair}' (expected K=V)");
        };
        env.insert(key.to_string(), value.to_string());
    }
    Ok(env)
}

/// Where a merged-view row was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpSource {
    /// newt's own `[[mcp_servers]]` (user + project config layers).
    NewtConfig,
    /// Claude Code user config: `~/.claude.json` → `mcpServers`.
    ClaudeUser,
    /// Claude Code project config: `./.mcp.json` → `mcpServers`.
    ClaudeProject,
}

impl McpSource {
    fn label(self) -> &'static str {
        match self {
            Self::NewtConfig => "newt config",
            Self::ClaudeUser => "claude-code (user)",
            Self::ClaudeProject => "claude-code (project)",
        }
    }
}

/// One row of the `newt mcp list` view.
#[derive(Debug, Clone, PartialEq, Eq)]
struct McpRow {
    name: String,
    transport: TransportKind,
    enabled: bool,
    source: McpSource,
    /// Mirrors [`McpServerEntry::is_valid`] — `false` rows are flagged in the
    /// rendering because discovery will drop them at session start.
    valid: bool,
}

/// Fold the three sources into the deduped view, mirroring
/// [`newt_core::mcp::discover`]'s precedence (newt > Claude user > Claude
/// project). Like discover, only VALID entries claim a name — a later valid
/// duplicate is shadowed, but an invalid entry never shadows the entry the
/// session will actually connect. Invalid entries are always shown, flagged.
/// Pure.
fn merged_rows(
    newt: &[McpServerEntry],
    claude_user: &[McpServerEntry],
    claude_project: &[McpServerEntry],
) -> Vec<McpRow> {
    let sources = [
        (McpSource::NewtConfig, newt),
        (McpSource::ClaudeUser, claude_user),
        (McpSource::ClaudeProject, claude_project),
    ];
    let mut claimed: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut rows = Vec::new();
    for (source, entries) in sources {
        for entry in entries {
            let valid = entry.is_valid();
            if valid && !claimed.insert(entry.name.as_str()) {
                continue; // shadowed by an earlier valid claimant
            }
            rows.push(McpRow {
                name: entry.name.clone(),
                transport: entry.transport,
                enabled: entry.enabled,
                source,
                valid,
            });
        }
    }
    rows
}

/// Render the view. Pure over the sink.
fn render_rows(rows: &[McpRow], out: &mut dyn Write) -> anyhow::Result<()> {
    if rows.is_empty() {
        writeln!(
            out,
            "No MCP servers configured. Register one with `newt mcp add <name> \
             --command <cmd>`, or `newt mcp install <name>` from the catalog."
        )?;
        return Ok(());
    }
    let width = rows
        .iter()
        .map(|r| r.name.len())
        .chain(["NAME".len()])
        .max()
        .unwrap_or(4);
    writeln!(
        out,
        "{:width$}  {:9}  {:7}  SOURCE",
        "NAME", "TRANSPORT", "ENABLED"
    )?;
    for row in rows {
        let note = if row.valid {
            ""
        } else {
            "  (invalid — dropped at discovery; fix or remove it)"
        };
        writeln!(
            out,
            "{:width$}  {:9}  {:7}  {}{note}",
            row.name,
            row.transport.as_str(),
            if row.enabled { "yes" } else { "no" },
            row.source.label(),
        )?;
    }
    Ok(())
}

/// IO shell for `newt mcp list`: load the newt config + the Claude Code
/// overlay files, then render the pure merged view.
fn cmd_list(config_path: Option<&Path>, out: &mut dyn Write) -> anyhow::Result<()> {
    // A broken newt config must fail loudly — an empty view over a config
    // that failed to parse would contradict the show-and-flag contract.
    // (resolve() returns the default config when NO file exists; it only
    // errors when a file is present but unreadable.)
    let cfg = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::resolve()?,
    };
    let claude_user = crate::home_dir()
        .map(|h| load_claude_json(&h.join(".claude.json")))
        .unwrap_or_default();
    let claude_project = std::env::current_dir()
        .map(|d| load_claude_json(&d.join(".mcp.json")))
        .unwrap_or_default();
    let rows = merged_rows(&cfg.mcp_servers, &claude_user, &claude_project);
    render_rows(&rows, out)
}

/// Best-effort read of a Claude-format `mcpServers` file — missing or
/// malformed yields an empty list, the same contract as discovery.
fn load_claude_json(path: &Path) -> Vec<McpServerEntry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .map(|value| newt_core::mcp::parse_claude_mcp(&value))
        .unwrap_or_default()
}

/// Where a resolved catalog entry came from — so an error about a broken
/// entry can name the file to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CatalogOrigin {
    Bundled,
    DropIn(PathBuf),
}

impl std::fmt::Display for CatalogOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bundled => write!(f, "the bundled catalog"),
            Self::DropIn(path) => write!(f, "{}", path.display()),
        }
    }
}

/// The server registration a catalog entry installs, validated. Pure. A
/// broken drop-in entry (e.g. a stdio server with no `command`) errors here,
/// naming the entry and the catalog it came from — not deep in the config
/// writer with no provenance.
fn installable_server(
    chosen: &McpCatalogEntry,
    origin: &CatalogOrigin,
) -> anyhow::Result<McpServerEntry> {
    let server = chosen.server();
    if !server.is_valid() {
        let need = match server.transport {
            TransportKind::Stdio => "a `command`",
            TransportKind::Sse | TransportKind::Http => "a `url`",
        };
        bail!(
            "catalog entry `{}` (from {origin}) is not installable: a {} server requires {need}",
            chosen.name,
            server.transport.as_str()
        );
    }
    Ok(server)
}

/// Resolve the effective catalog: bundled < `~/.newt/mcp-catalog.toml` <
/// `.newt/mcp-catalog.toml`, merged by name with each entry keeping the
/// origin of the layer that won it. A present-but-malformed drop-in is a
/// loud error (installing from a half-read catalog would be worse), a
/// missing one is simply skipped.
fn resolve_catalog() -> anyhow::Result<Vec<(McpCatalogEntry, CatalogOrigin)>> {
    let mut layers = vec![(CatalogOrigin::Bundled, builtin_catalog())];
    let user = Config::user_config_dir().map(|d| d.join("mcp-catalog.toml"));
    let project = std::env::current_dir()
        .ok()
        .map(|d| d.join(".newt").join("mcp-catalog.toml"));
    for path in [user, project].into_iter().flatten() {
        if let Some(text) = read_optional(&path)? {
            let entries = parse_catalog(&text).with_context(|| format!("in {}", path.display()))?;
            layers.push((CatalogOrigin::DropIn(path), entries));
        }
    }
    Ok(merge_catalog_layers(layers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn stdio_entry(name: &str, command: Option<&str>) -> McpServerEntry {
        McpServerEntry {
            name: name.into(),
            enabled: true,
            transport: TransportKind::Stdio,
            command: command.map(str::to_string),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            request_timeout_secs: None,
        }
    }

    // Backward compatibility: bare `newt mcp` must still parse to NO
    // subcommand — the serve-over-stdio mode.
    #[test]
    fn bare_mcp_parses_to_serve_mode() {
        let cli = crate::Cli::try_parse_from(["newt", "mcp"]).unwrap();
        assert!(
            matches!(cli.command, Some(crate::Command::Mcp { cmd: None })),
            "bare `newt mcp` must keep serving over stdio"
        );
    }

    #[test]
    fn mcp_add_parses_repeatable_args_and_env() {
        let cli = crate::Cli::try_parse_from([
            "newt",
            "mcp",
            "add",
            "scrybe",
            "--command",
            "scrybe-mcp-server",
            "--arg",
            "stdio",
            "--arg",
            "--verbose",
            "--env",
            "A=1",
            "--env",
            "B=2",
            "--timeout-secs",
            "90",
            "--project",
        ])
        .unwrap();
        let Some(crate::Command::Mcp {
            cmd:
                Some(McpCmd::Add {
                    name,
                    command,
                    args,
                    transport,
                    url,
                    env,
                    timeout_secs,
                    project,
                }),
        }) = cli.command
        else {
            panic!("expected mcp add");
        };
        assert_eq!(name, "scrybe");
        assert_eq!(command.as_deref(), Some("scrybe-mcp-server"));
        assert_eq!(args, vec!["stdio", "--verbose"]);
        assert_eq!(transport, TransportKind::Stdio, "stdio is the default");
        assert_eq!(url, None);
        assert_eq!(env, vec!["A=1", "B=2"]);
        assert_eq!(timeout_secs, Some(90));
        assert!(project);
    }

    #[test]
    fn mcp_add_rejects_an_unknown_transport() {
        let err = crate::Cli::try_parse_from(["newt", "mcp", "add", "x", "--transport", "grpc"])
            .unwrap_err();
        assert!(err.to_string().contains("stdio, sse, http"), "{err}");
    }

    #[test]
    fn build_entry_requires_the_transport_matched_endpoint() {
        // stdio without --command.
        let err = build_entry(
            "x".into(),
            None,
            vec![],
            TransportKind::Stdio,
            None,
            &[],
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--command"), "{err}");
        // sse without --url.
        let err = build_entry(
            "x".into(),
            None,
            vec![],
            TransportKind::Sse,
            None,
            &[],
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--url"), "{err}");
        // --url on a stdio server is a mistake, not silently dropped noise.
        let err = build_entry(
            "x".into(),
            Some("cmd".into()),
            vec![],
            TransportKind::Stdio,
            Some("https://x".into()),
            &[],
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--url"), "{err}");
        // --command on an http server likewise.
        let err = build_entry(
            "x".into(),
            Some("cmd".into()),
            vec![],
            TransportKind::Http,
            Some("https://x".into()),
            &[],
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--command"), "{err}");
    }

    #[test]
    fn build_entry_assembles_a_full_stdio_registration() {
        let entry = build_entry(
            "scrybe".into(),
            Some("scrybe-mcp-server".into()),
            vec!["stdio".into()],
            TransportKind::Stdio,
            None,
            &["SCRYBE_LOG=info".to_string()],
            Some(120),
        )
        .unwrap();
        assert_eq!(entry.name, "scrybe");
        assert!(entry.enabled);
        assert_eq!(entry.command.as_deref(), Some("scrybe-mcp-server"));
        assert_eq!(entry.args, vec!["stdio"]);
        assert_eq!(
            entry.env.get("SCRYBE_LOG").map(String::as_str),
            Some("info")
        );
        assert_eq!(entry.request_timeout_secs, Some(120));
        assert!(entry.is_valid());
    }

    #[test]
    fn env_pairs_split_on_the_first_equals_and_reject_malformed() {
        let got = parse_env_pairs(&["A=1".into(), "B=x=y".into(), "EMPTY=".into()]).unwrap();
        assert_eq!(got.get("A").map(String::as_str), Some("1"));
        assert_eq!(got.get("B").map(String::as_str), Some("x=y"));
        assert_eq!(got.get("EMPTY").map(String::as_str), Some(""));
        assert!(parse_env_pairs(&["NOEQUALS".into()]).is_err());
        assert!(parse_env_pairs(&["=value".into()]).is_err());
    }

    #[test]
    fn installable_server_names_the_entry_and_the_catalog_it_came_from() {
        // A drop-in entry that parses but can never connect must error at
        // install time, pointing at the file to fix — not surface as a bare
        // config-writer error with no provenance.
        let broken = &parse_catalog("[[servers]]\nname = \"half\"\n").unwrap()[0];
        let origin = CatalogOrigin::DropIn(PathBuf::from("/proj/.newt/mcp-catalog.toml"));
        let err = installable_server(broken, &origin).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("half"), "names the entry: {msg}");
        assert!(msg.contains("mcp-catalog.toml"), "names the file: {msg}");
        assert!(msg.contains("command"), "names the missing field: {msg}");
        // Bundled origin is named as such.
        let err = installable_server(broken, &CatalogOrigin::Bundled).unwrap_err();
        assert!(err.to_string().contains("bundled"), "{err}");
        // A valid entry passes through with its name filled in.
        let good = &parse_catalog("[[servers]]\nname = \"ok\"\ncommand = \"ok-mcp\"\n").unwrap()[0];
        let server = installable_server(good, &origin).unwrap();
        assert_eq!(server.name, "ok");
        assert_eq!(server.command.as_deref(), Some("ok-mcp"));
    }

    #[test]
    fn merged_rows_dedup_by_precedence_and_flag_invalid() {
        let newt = vec![
            stdio_entry("dup", Some("newt-wins")),
            stdio_entry("broken", None), // invalid: stdio, no command
        ];
        let claude_user = vec![stdio_entry("dup", Some("shadowed")), {
            let mut e = stdio_entry("user-only", Some("u"));
            e.enabled = false;
            e
        }];
        let claude_project = vec![stdio_entry("proj-only", Some("p"))];
        let rows = merged_rows(&newt, &claude_user, &claude_project);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["dup", "broken", "user-only", "proj-only"]);
        assert_eq!(rows[0].source, McpSource::NewtConfig, "newt wins the dup");
        assert!(!rows[1].valid, "invalid entries are shown, flagged");
        assert_eq!(rows[2].source, McpSource::ClaudeUser);
        assert!(!rows[2].enabled);
        assert_eq!(rows[3].source, McpSource::ClaudeProject);
    }

    #[test]
    fn merged_rows_never_let_an_invalid_entry_shadow_the_real_winner() {
        // discover() only lets VALID entries claim a name: with an invalid
        // newt "x" and a valid claude-code "x", the session connects the
        // claude one. The view must show BOTH — the invalid row flagged, and
        // the valid row that actually wins — never hide the winner.
        let newt = vec![stdio_entry("x", None)]; // invalid: stdio, no command
        let claude_user = vec![stdio_entry("x", Some("claude-wins"))];
        let rows = merged_rows(&newt, &claude_user, &[]);
        assert_eq!(
            rows.len(),
            2,
            "both the flagged row and the winner: {rows:?}"
        );
        assert_eq!(rows[0].source, McpSource::NewtConfig);
        assert!(!rows[0].valid);
        assert_eq!(rows[1].source, McpSource::ClaudeUser);
        assert!(rows[1].valid, "the connecting entry must be visible");
        // A valid claimant still shadows a later VALID duplicate.
        let rows = merged_rows(
            &[stdio_entry("y", Some("newt-wins"))],
            &[stdio_entry("y", Some("shadowed"))],
            &[],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].source, McpSource::NewtConfig);
    }

    #[test]
    fn render_rows_lists_name_transport_enabled_and_source() {
        let rows = merged_rows(
            &[stdio_entry("scrybe", Some("scrybe-mcp-server"))],
            &[stdio_entry("broken", None)],
            &[],
        );
        let mut out = Vec::new();
        render_rows(&rows, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("scrybe"), "{text}");
        assert!(text.contains("stdio"), "{text}");
        assert!(text.contains("yes"), "{text}");
        assert!(text.contains("newt config"), "{text}");
        assert!(text.contains("claude-code (user)"), "{text}");
        assert!(
            text.contains("invalid"),
            "an unconnectable entry is flagged: {text}"
        );
    }

    #[test]
    fn render_rows_empty_view_points_at_add_and_install() {
        let mut out = Vec::new();
        render_rows(&[], &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("newt mcp add"), "{text}");
        assert!(text.contains("newt mcp install"), "{text}");
    }
}
