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
use newt_core::mcp::{McpServerEntry, SecretValue, TransportKind};
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
    /// Run newt as an MCP server over stdio (JSON-RPC, no TUI) — always,
    /// regardless of whether stdin is a terminal. This is the explicit,
    /// unambiguous manual way to serve; bare `newt mcp` serves too when its
    /// stdin is piped (an MCP client), but prints this menu at a terminal.
    Serve,
    /// Import MCP servers from a Claude-Code `mcpServers` JSON, writing the
    /// equivalent newt TOML — breaking config out to `~/.newt/mcp.toml`
    /// (created if absent). Secret-bearing values (incl. Claude's `${VAR}`) are
    /// imported verbatim; newt resolves `${…}` host-side at spawn.
    Import {
        /// Path to a Claude-format JSON (`~/.claude.json`, a project `.mcp.json`,
        /// or a pasted `mcp.json`). Omit when using `--from-claude`.
        #[arg(required_unless_present = "from_claude")]
        path: Option<PathBuf>,
        /// Import from `~/.claude.json`.
        #[arg(long = "from-claude")]
        from_claude: bool,
        /// Overwrite entries whose name already exists (default: error on a clash).
        #[arg(long)]
        force: bool,
        /// Keep existing entries — import only names not already present (no error).
        #[arg(long, conflicts_with = "force")]
        merge: bool,
        /// Write to the project config instead of the user `~/.newt/mcp.toml`.
        #[arg(long)]
        project: bool,
    },
}

/// What bare `newt mcp` (no subcommand) should do, decided purely from
/// whether stdin is a terminal. Factored out so the choice is unit-testable
/// without a real TTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BareMcpAction {
    /// stdin is piped (an MCP client spawned us, or the stdout-purity
    /// tests): serve over stdio exactly as before — the backward-compatible
    /// path every `claude mcp add newt -- newt mcp` config relies on.
    Serve,
    /// stdin is an interactive terminal: a human typed `newt mcp` expecting
    /// the management verbs. Print the subcommand menu instead of blocking
    /// as a server that reads stdin forever.
    Help,
}

/// Decide what bare `newt mcp` should do from the TTY-ness of stdin.
///
/// Piped stdin ⇒ [`BareMcpAction::Serve`]; a terminal ⇒
/// [`BareMcpAction::Help`]. Dispatched in `lib.rs`, which passes
/// `std::io::stdin().is_terminal()`.
pub fn bare_mcp_action(stdin_is_tty: bool) -> BareMcpAction {
    if stdin_is_tty {
        BareMcpAction::Help
    } else {
        BareMcpAction::Serve
    }
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
            let path = remove_server(&name, config_path, project)?;
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
            let mut server = installable_server(chosen, origin)?;
            // scrybe.ai "special relationship": resolve the binary to an
            // absolute path (PATH → ~/venv/bin) so the registration survives
            // PATH changes; a missing bundled-scrybe binary is a clear pip hint.
            finalize_install_command(
                &mut server,
                &chosen.name,
                matches!(origin, CatalogOrigin::Bundled),
            )?;
            let path = add_to_config(&server, config_path, project)?;
            writeln!(
                out,
                "Installed MCP server '{}' ({}) in {}",
                chosen.name,
                chosen.description,
                path.display()
            )?;
            if let Some(cmd) = &server.command {
                if cmd.contains(std::path::MAIN_SEPARATOR) {
                    writeln!(out, "Resolved command to {cmd}")?;
                }
            }
            print_next_steps(&mut out)
        }
        // `serve` runs newt-as-a-server, which needs the persona (a global
        // flag) and the stdio_guard redirect that only `run_mcp` in lib.rs
        // owns. lib.rs intercepts `Some(McpCmd::Serve)` before delegating
        // here, so this arm is never reached.
        McpCmd::Serve => unreachable!("`newt mcp serve` is dispatched to run_mcp in lib.rs"),
        McpCmd::Import {
            path,
            from_claude,
            force,
            merge,
            project,
        } => cmd_import(
            path.as_deref(),
            from_claude,
            force,
            merge,
            config_path,
            project,
            &mut out,
        ),
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
    if let Some(explicit) = explicit_write_target(config_path, project)? {
        return Ok(explicit);
    }
    Config::user_config_path().ok_or_else(|| anyhow!("cannot resolve ~/.newt (no home dir)"))
}

/// The explicit, non-user-level write targets every mcp verb shares:
/// `--project` > `--config` > `$NEWT_CONFIG` > `./newt.toml`. Returns `Ok(None)`
/// when none apply — the caller then resolves the user-level target (either the
/// user `config.toml` or the broken-out `~/.newt/mcp.toml`).
pub(crate) fn explicit_write_target(
    config_path: Option<&Path>,
    project: bool,
) -> anyhow::Result<Option<PathBuf>> {
    if project {
        if let Some(existing) = Config::project_config_path() {
            return Ok(Some(existing));
        }
        return Ok(Some(
            std::env::current_dir()
                .context("cannot resolve the current directory")?
                .join(".newt")
                .join("config.toml"),
        ));
    }
    if let Some(explicit) = config_path {
        return Ok(Some(explicit.to_path_buf()));
    }
    if let Some(env_cfg) = std::env::var_os("NEWT_CONFIG").filter(|v| !v.is_empty()) {
        return Ok(Some(PathBuf::from(env_cfg)));
    }
    let local = std::env::current_dir()
        .context("cannot resolve the current directory")?
        .join("newt.toml");
    if local.is_file() {
        return Ok(Some(local));
    }
    Ok(None)
}

/// The write target for `newt mcp add|install|import`, extending
/// [`write_target`] with the broken-out `~/.newt/mcp.toml` preference. Explicit
/// targets (`--project` / `--config` / `$NEWT_CONFIG` / `./newt.toml`) win first,
/// exactly as #1291. Otherwise, at the user level, the newt-owned
/// `~/.newt/mcp.toml` is preferred when it already **exists** — or **created**,
/// when `create` is set (the `import` / break-out gesture) — else the user
/// `config.toml` (#1291's current behavior).
pub(crate) fn mcp_write_target(
    config_path: Option<&Path>,
    project: bool,
    create: bool,
) -> anyhow::Result<PathBuf> {
    if let Some(explicit) = explicit_write_target(config_path, project)? {
        return Ok(explicit);
    }
    let dir =
        Config::user_config_dir().ok_or_else(|| anyhow!("cannot resolve ~/.newt (no home dir)"))?;
    let mcp_toml = dir.join("mcp.toml");
    if create || mcp_toml.is_file() {
        return Ok(mcp_toml);
    }
    Ok(dir.join("config.toml"))
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
    // `add`/`install` prefer an EXISTING `~/.newt/mcp.toml` (create=false) — they
    // never spontaneously break config out; only `import` does that.
    let path = mcp_write_target(config_path, project, false)?;
    let text = read_config_text(&path)?;
    let updated = Config::with_mcp_server_added(&text, entry)?;
    write_back(&path, &updated)?;
    Ok(path)
}

/// Write `text` to `path`, creating parent dirs. Shared by every mcp write verb.
fn write_back(path: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

/// Whether TOML `text` carries a `[[mcp_servers]]` entry named `name`.
fn config_has_mcp_server(text: &str, name: &str) -> bool {
    newt_core::mcp::parse_newt_mcp_toml(text)
        .iter()
        .any(|e| e.name == name)
}

/// Remove `name`, resolving the target the same way `add` does. An explicit
/// target (`--project`/`--config`/`$NEWT_CONFIG`/`./newt.toml`) is acted on
/// directly. At the user level the entry may live in either the broken-out
/// `~/.newt/mcp.toml` or `~/.newt/config.toml`, so the first of those that
/// actually contains the name is edited — a `remove` never fails just because
/// the operator has broken some servers out and left others inline.
fn remove_server(name: &str, config_path: Option<&Path>, project: bool) -> anyhow::Result<PathBuf> {
    if let Some(explicit) = explicit_write_target(config_path, project)? {
        let text = read_config_text(&explicit)?;
        let updated = Config::with_mcp_server_removed(&text, name)?;
        write_back(&explicit, &updated)?;
        return Ok(explicit);
    }
    let dir =
        Config::user_config_dir().ok_or_else(|| anyhow!("cannot resolve ~/.newt (no home dir)"))?;
    for candidate in [dir.join("mcp.toml"), dir.join("config.toml")] {
        if let Some(text) = read_optional(&candidate)? {
            if config_has_mcp_server(&text, name) {
                let updated = Config::with_mcp_server_removed(&text, name)?;
                write_back(&candidate, &updated)?;
                return Ok(candidate);
            }
        }
    }
    bail!("no MCP server named `{name}` in ~/.newt/mcp.toml or ~/.newt/config.toml");
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
/// empty key; the value may be empty (explicitly unsetting is legitimate). Each
/// value becomes a [`SecretValue::Literal`] — so an operator can pass a `${...}`
/// interpolation (`--env "TOKEN=${cmd:vault kv get …}"`), resolved host-side at
/// spawn.
pub(crate) fn parse_env_pairs(pairs: &[String]) -> anyhow::Result<BTreeMap<String, SecretValue>> {
    let mut env = BTreeMap::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=').filter(|(k, _)| !k.is_empty()) else {
            bail!("invalid --env '{pair}' (expected K=V)");
        };
        env.insert(key.to_string(), SecretValue::literal(value));
    }
    Ok(env)
}

/// Where a merged-view row was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpSource {
    /// newt's own `[[mcp_servers]]` (user + project config layers).
    NewtConfig,
    /// The newt-owned broken-out source: `~/.newt/mcp.toml`.
    NewtMcpToml,
    /// Claude Code user config: `~/.claude.json` → `mcpServers`.
    ClaudeUser,
    /// Claude Code project config: `./.mcp.json` → `mcpServers`.
    ClaudeProject,
}

impl McpSource {
    fn label(self) -> &'static str {
        match self {
            Self::NewtConfig => "newt config",
            Self::NewtMcpToml => "newt mcp.toml",
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
    mcp_toml: &[McpServerEntry],
    claude_user: &[McpServerEntry],
    claude_project: &[McpServerEntry],
) -> Vec<McpRow> {
    let sources = [
        (McpSource::NewtConfig, newt),
        (McpSource::NewtMcpToml, mcp_toml),
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
    let mcp_toml = Config::user_config_dir()
        .map(|d| load_newt_mcp_toml(&d.join("mcp.toml")))
        .unwrap_or_default();
    let claude_user = crate::home_dir()
        .map(|h| load_claude_json(&h.join(".claude.json")))
        .unwrap_or_default();
    let claude_project = std::env::current_dir()
        .map(|d| load_claude_json(&d.join(".mcp.json")))
        .unwrap_or_default();
    let rows = merged_rows(&cfg.mcp_servers, &mcp_toml, &claude_user, &claude_project);
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

/// Best-effort read of a newt-owned `~/.newt/mcp.toml` (`[[mcp_servers]]`) —
/// missing or malformed yields an empty list, the same contract as discovery.
fn load_newt_mcp_toml(path: &Path) -> Vec<McpServerEntry> {
    read_optional(path)
        .ok()
        .flatten()
        .map(|text| newt_core::mcp::parse_newt_mcp_toml(&text))
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

// ---------------------------------------------------------------------------
// `newt mcp import` — Claude JSON → newt TOML bridge
// ---------------------------------------------------------------------------

/// Resolve the JSON source for `import`: `--from-claude` → `~/.claude.json`,
/// otherwise the explicit `path`.
fn resolve_import_source(path: Option<&Path>, from_claude: bool) -> anyhow::Result<PathBuf> {
    if from_claude {
        let home =
            crate::home_dir().ok_or_else(|| anyhow!("cannot resolve $HOME for --from-claude"))?;
        return Ok(home.join(".claude.json"));
    }
    match path {
        Some(p) => Ok(p.to_path_buf()),
        None => bail!("provide a path to a Claude mcpServers JSON, or pass --from-claude"),
    }
}

/// `newt mcp import` — read a Claude-Code `mcpServers` JSON and write each server
/// as a `[[mcp_servers]]` block via the comment-preserving writer. Dedup-by-name:
/// a clash is an error by default, `--force` overwrites, `--merge` skips existing.
/// Default target is the broken-out `~/.newt/mcp.toml` (created if absent).
fn cmd_import(
    path: Option<&Path>,
    from_claude: bool,
    force: bool,
    merge: bool,
    config_path: Option<&Path>,
    project: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let source = resolve_import_source(path, from_claude)?;
    let text = std::fs::read_to_string(&source)
        .with_context(|| format!("reading {}", source.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", source.display()))?;
    let mut imported = newt_core::mcp::parse_claude_mcp(&value);
    if imported.is_empty() {
        bail!("no `mcpServers` entries found in {}", source.display());
    }
    // Deterministic order (map iteration is not) so output + writes are stable.
    imported.sort_by(|a, b| a.name.cmp(&b.name));

    // import is the break-out gesture: create ~/.newt/mcp.toml when at the user
    // level (create=true), unless an explicit target overrides.
    let target = mcp_write_target(config_path, project, true)?;
    let mut doc = read_config_text(&target)?;
    let existing: std::collections::BTreeSet<String> = newt_core::mcp::parse_newt_mcp_toml(&doc)
        .into_iter()
        .map(|e| e.name)
        .collect();

    let mut added = 0usize;
    let mut overwritten = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    for entry in &imported {
        if existing.contains(&entry.name) {
            if merge {
                skipped.push(entry.name.clone());
                continue;
            }
            if force {
                doc = Config::with_mcp_server_removed(&doc, &entry.name)?;
                doc = Config::with_mcp_server_added(&doc, entry)?;
                overwritten += 1;
                continue;
            }
            bail!(
                "MCP server `{}` already exists in {} — use --force to overwrite, \
                 or --merge to skip existing",
                entry.name,
                target.display()
            );
        }
        doc = Config::with_mcp_server_added(&doc, entry)?;
        added += 1;
    }
    write_back(&target, &doc)?;

    writeln!(
        out,
        "Imported {added} MCP server(s) from {} into {}",
        source.display(),
        target.display()
    )?;
    if overwritten > 0 {
        writeln!(out, "Overwrote {overwritten} existing server(s).")?;
    }
    if !skipped.is_empty() {
        writeln!(
            out,
            "Skipped {} already present: {}",
            skipped.len(),
            skipped.join(", ")
        )?;
    }
    print_next_steps(out)
}

// ---------------------------------------------------------------------------
// scrybe.ai smart-install — binary resolution
// ---------------------------------------------------------------------------

/// Build the ordered candidate paths for `command`: every `$PATH` dir first,
/// then `~/venv/bin`. Pure over the injected dirs so the resolution ORDER is
/// unit-tested without touching the real filesystem/PATH.
fn binary_candidates_in(
    path_dirs: &[PathBuf],
    venv_bin: Option<&Path>,
    command: &str,
) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = path_dirs.iter().map(|d| d.join(command)).collect();
    if let Some(v) = venv_bin {
        candidates.push(v.join(command));
    }
    candidates
}

/// The first candidate that `exists`. Pure — order + existence predicate both
/// injected, so "PATH before ~/venv, earliest present wins" is unit-tested.
fn first_existing(candidates: &[PathBuf], exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    candidates.iter().find(|p| exists(p)).cloned()
}

/// Live candidate list for `command`: every `$PATH` entry, then `~/venv/bin`.
fn install_binary_candidates(command: &str) -> Vec<PathBuf> {
    let path_dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let venv_bin = crate::home_dir().map(|h| h.join("venv").join("bin"));
    binary_candidates_in(&path_dirs, venv_bin.as_deref(), command)
}

/// Resolve a stdio catalog entry's bare `command` to an absolute path (PATH →
/// `~/venv/bin`), so the registration survives PATH changes. For the blessed
/// **bundled** `scrybe` entry an unresolved binary is a hard error naming the
/// pip package (the "special relationship" — remove the setup friction). Any
/// other entry — including a user/project drop-in that overrides scrybe — keeps
/// its bare command when the binary is absent (its author owns resolution). A
/// command that already carries a path separator is respected as-is.
fn finalize_install_command(
    server: &mut McpServerEntry,
    catalog_name: &str,
    bundled: bool,
) -> anyhow::Result<()> {
    if server.transport != TransportKind::Stdio {
        return Ok(());
    }
    let Some(command) = server.command.clone() else {
        return Ok(());
    };
    if command.contains(std::path::MAIN_SEPARATOR) {
        return Ok(());
    }
    match first_existing(&install_binary_candidates(&command), |p| p.is_file()) {
        Some(abs) => {
            server.command = Some(abs.to_string_lossy().into_owned());
            Ok(())
        }
        None if catalog_name == "scrybe" && bundled => bail!(
            "`{command}` was not found on your PATH or in ~/venv/bin.\n\
             Install the Scrybe MCP server with `pip install scrybe.ai` (it provides \
             `{command}`), then re-run `newt mcp install scrybe`."
        ),
        None => Ok(()),
    }
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

    // The explicit, unambiguous manual serve verb: `newt mcp serve`.
    #[test]
    fn mcp_serve_parses_to_serve_variant() {
        let cli = crate::Cli::try_parse_from(["newt", "mcp", "serve"]).unwrap();
        assert!(
            matches!(
                cli.command,
                Some(crate::Command::Mcp {
                    cmd: Some(McpCmd::Serve)
                })
            ),
            "`newt mcp serve` must parse to the Serve variant"
        );
    }

    // TTY seam: a piped stdin (an MCP client, or the stdout-purity tests)
    // means SERVE — the backward-compatible path.
    #[test]
    fn bare_mcp_action_serves_when_stdin_is_piped() {
        assert_eq!(bare_mcp_action(false), BareMcpAction::Serve);
    }

    // TTY seam: an interactive human at a terminal gets the verb menu, not
    // a server that blocks on stdin.
    #[test]
    fn bare_mcp_action_prints_help_when_stdin_is_a_terminal() {
        assert_eq!(bare_mcp_action(true), BareMcpAction::Help);
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
            entry
                .env
                .get("SCRYBE_LOG")
                .and_then(SecretValue::as_literal),
            Some("info")
        );
        assert_eq!(entry.request_timeout_secs, Some(120));
        assert!(entry.is_valid());
    }

    #[test]
    fn env_pairs_split_on_the_first_equals_and_reject_malformed() {
        let got = parse_env_pairs(&["A=1".into(), "B=x=y".into(), "EMPTY=".into()]).unwrap();
        assert_eq!(got.get("A").and_then(SecretValue::as_literal), Some("1"));
        assert_eq!(got.get("B").and_then(SecretValue::as_literal), Some("x=y"));
        assert_eq!(got.get("EMPTY").and_then(SecretValue::as_literal), Some(""));
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
        let mcp_toml = vec![
            stdio_entry("brokenout", Some("bo")),
            stdio_entry("dup", Some("shadowed-by-config")),
        ];
        let claude_project = vec![stdio_entry("proj-only", Some("p"))];
        let rows = merged_rows(&newt, &mcp_toml, &claude_user, &claude_project);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["dup", "broken", "brokenout", "user-only", "proj-only"]
        );
        assert_eq!(rows[0].source, McpSource::NewtConfig, "config wins the dup");
        assert!(!rows[1].valid, "invalid entries are shown, flagged");
        assert_eq!(
            rows[2].source,
            McpSource::NewtMcpToml,
            "the broken-out mcp.toml source is attributed"
        );
        assert_eq!(rows[3].source, McpSource::ClaudeUser);
        assert!(!rows[3].enabled);
        assert_eq!(rows[4].source, McpSource::ClaudeProject);
    }

    #[test]
    fn merged_rows_never_let_an_invalid_entry_shadow_the_real_winner() {
        // discover() only lets VALID entries claim a name: with an invalid
        // newt "x" and a valid claude-code "x", the session connects the
        // claude one. The view must show BOTH — the invalid row flagged, and
        // the valid row that actually wins — never hide the winner.
        let newt = vec![stdio_entry("x", None)]; // invalid: stdio, no command
        let claude_user = vec![stdio_entry("x", Some("claude-wins"))];
        let rows = merged_rows(&newt, &[], &claude_user, &[]);
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
            &[],
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
            &[stdio_entry("brokenout", Some("bo-mcp"))],
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
        assert!(text.contains("newt mcp.toml"), "{text}");
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

    // ---- scrybe smart-install: binary resolution order (injected paths) ----

    #[test]
    fn binary_candidates_try_path_dirs_before_venv() {
        let cands = binary_candidates_in(
            &[PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")],
            Some(Path::new("/home/u/venv/bin")),
            "scrybe-mcp-server",
        );
        assert_eq!(
            cands,
            vec![
                PathBuf::from("/usr/local/bin/scrybe-mcp-server"),
                PathBuf::from("/usr/bin/scrybe-mcp-server"),
                PathBuf::from("/home/u/venv/bin/scrybe-mcp-server"),
            ]
        );
        // No venv → PATH candidates only.
        assert_eq!(
            binary_candidates_in(&[PathBuf::from("/bin")], None, "x"),
            vec![PathBuf::from("/bin/x")]
        );
    }

    #[test]
    fn first_existing_returns_the_earliest_present_candidate() {
        let cands = vec![
            PathBuf::from("/a/x"),               // missing → skipped
            PathBuf::from("/b/x"),               // present → the winner
            PathBuf::from("/home/u/venv/bin/x"), // present too, but later
        ];
        let present: std::collections::BTreeSet<PathBuf> =
            [PathBuf::from("/b/x"), PathBuf::from("/home/u/venv/bin/x")]
                .into_iter()
                .collect();
        assert_eq!(
            first_existing(&cands, |p| present.contains(p)),
            Some(PathBuf::from("/b/x")),
            "PATH resolves before ~/venv/bin; earliest present wins"
        );
        // Nothing present → None (the pip-hint path).
        assert_eq!(first_existing(&cands, |_| false), None);
    }

    #[test]
    fn finalize_install_leaves_an_explicit_path_and_non_stdio_untouched() {
        // A command that already carries a path separator is respected as-is —
        // even for the bundled scrybe entry.
        let mut abs = stdio_entry("scrybe", Some("/opt/scrybe/bin/scrybe-mcp-server"));
        finalize_install_command(&mut abs, "scrybe", true).unwrap();
        assert_eq!(
            abs.command.as_deref(),
            Some("/opt/scrybe/bin/scrybe-mcp-server")
        );
        // A non-stdio server has no binary to resolve.
        let mut http = McpServerEntry {
            transport: TransportKind::Http,
            command: None,
            url: Some("https://x/mcp".into()),
            ..stdio_entry("remote", None)
        };
        finalize_install_command(&mut http, "remote", true).unwrap();
        assert!(http.command.is_none());
    }

    // ---- `newt mcp import` source resolution ----

    #[test]
    fn import_source_resolves_explicit_path_and_requires_one() {
        assert_eq!(
            resolve_import_source(Some(Path::new("/tmp/mcp.json")), false).unwrap(),
            PathBuf::from("/tmp/mcp.json")
        );
        // Neither a path nor --from-claude → a loud usage error.
        assert!(resolve_import_source(None, false).is_err());
    }

    #[test]
    fn mcp_add_import_parses() {
        let cli =
            crate::Cli::try_parse_from(["newt", "mcp", "import", "/tmp/claude.json", "--force"])
                .unwrap();
        let Some(crate::Command::Mcp {
            cmd:
                Some(McpCmd::Import {
                    path,
                    from_claude,
                    force,
                    merge,
                    project,
                }),
        }) = cli.command
        else {
            panic!("expected mcp import");
        };
        assert_eq!(path.as_deref(), Some(Path::new("/tmp/claude.json")));
        assert!(!from_claude);
        assert!(force);
        assert!(!merge);
        assert!(!project);
        // --from-claude makes the path optional.
        assert!(crate::Cli::try_parse_from(["newt", "mcp", "import", "--from-claude"]).is_ok());
        // --force and --merge are mutually exclusive.
        assert!(crate::Cli::try_parse_from([
            "newt",
            "mcp",
            "import",
            "/tmp/c.json",
            "--force",
            "--merge"
        ])
        .is_err());
    }
}
