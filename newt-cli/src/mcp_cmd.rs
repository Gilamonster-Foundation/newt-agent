//! `newt mcp add|remove|list|install|import` — manage `[[mcp_servers]]`
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
//!
//! Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 14:20 EDT | Date: 2026-08-12

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use clap::Subcommand;
use newt_core::atomic_fs::{LockGuard, ResolvedPath};
use newt_core::config::ArrayMergeStrategy;
use newt_core::mcp::{McpImportParseReport, McpServerEntry, McpTrust, SecretValue, TransportKind};
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
    /// Import selected MCP servers from Claude Code, Codex, or a config file.
    /// The equivalent trusted newt TOML is written to `~/.newt/mcp.toml`
    /// (created if absent). Literal or active secret-bearing values make the
    /// selected import fail; environment references survive.
    Import {
        /// Path to a Claude JSON or Codex TOML MCP config. Omit with a built-in
        /// source flag.
        #[arg(
            required_unless_present_any = ["from_claude", "from_codex"],
            conflicts_with_all = ["from_claude", "from_codex"]
        )]
        path: Option<PathBuf>,
        /// Import from `~/.claude.json`.
        #[arg(long = "from-claude", conflicts_with = "from_codex")]
        from_claude: bool,
        /// Import from `$CODEX_HOME/config.toml` or `~/.codex/config.toml`.
        #[arg(long = "from-codex")]
        from_codex: bool,
        /// Import exactly one named server.
        #[arg(long, value_name = "NAME", required_unless_present = "all")]
        name: Option<String>,
        /// Import every server in the selected source.
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// Grant only each imported HTTP server's exact hostname in
        /// `[tui.permissions] net`.
        #[arg(long)]
        grant_net: bool,
        /// Overwrite entries whose name already exists (default: error on a clash).
        #[arg(long)]
        force: bool,
        /// Keep existing entries — import only names not already present (no error).
        #[arg(long, conflicts_with = "force")]
        merge: bool,
        /// Rejected for imports: project config is borrowed and untrusted.
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
                if cmd.contains(['/', '\\']) {
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
            from_codex,
            name,
            all,
            grant_net,
            force,
            merge,
            project,
        } => cmd_import(
            ImportRequest {
                path: path.as_deref(),
                from_claude,
                from_codex,
                name: name.as_deref(),
                all,
                grant_net,
                force,
                merge,
                config_path,
                project,
            },
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
    if let Some(strong) = strong_explicit_write_target(config_path, project)? {
        return Ok(Some(strong));
    }
    ambient_newt_toml()
}

/// The **strong** explicit write targets — deliberate overrides that always win:
/// `--project` > `--config` > `$NEWT_CONFIG`. Split out from
/// [`explicit_write_target`] so [`mcp_write_target`]'s `create=true` (import /
/// break-out) path can honor these WITHOUT the ambient `./newt.toml`
/// fallthrough capturing a plain `newt mcp import` (FIX 4, #1301). `Ok(None)`
/// when none apply.
fn strong_explicit_write_target(
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
    Ok(None)
}

/// The ambient `./newt.toml` fallthrough (resolve()'s next base candidate) — the
/// weak, cwd-dependent target. `Ok(None)` when the cwd has no `newt.toml`.
fn ambient_newt_toml() -> anyhow::Result<Option<PathBuf>> {
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
    // A STRONG explicit override always wins. The ambient `./newt.toml` is NOT
    // consulted before the user-global target on the create path — otherwise a
    // plain `newt mcp import` run from a project dir that happens to hold a
    // `newt.toml` would silently scope a user-global import to that project
    // (FIX 4, #1301).
    if let Some(strong) = strong_explicit_write_target(config_path, project)? {
        return Ok(strong);
    }
    let dir =
        Config::user_config_dir().ok_or_else(|| anyhow!("cannot resolve ~/.newt (no home dir)"))?;
    let mcp_toml = dir.join("mcp.toml");
    if create {
        // The break-out/import gesture targets `~/.newt/mcp.toml` as its help
        // promises — created if absent, never captured by an ambient newt.toml.
        return Ok(mcp_toml);
    }
    // add/install (create=false): #1291 behavior preserved — an ambient
    // `./newt.toml` (resolve()'s base) first, then an existing mcp.toml, else the
    // user `config.toml`.
    if let Some(ambient) = ambient_newt_toml()? {
        return Ok(ambient);
    }
    if mcp_toml.is_file() {
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
    let (destination, _guard) = resolve_and_lock_write_target(&path)?;
    let text = read_config_text(destination.as_path())?;
    let updated = Config::with_mcp_server_added(&text, entry)?;
    write_back(&destination, &updated)?;
    Ok(path)
}

/// Bind and lock a config target before a read-modify-write operation. The
/// returned guard shares the same owner-aware sidecar used by setup, permanent
/// permission grants, and MCP imports.
fn resolve_and_lock_write_target(path: &Path) -> anyhow::Result<(ResolvedPath, LockGuard)> {
    let destination = ResolvedPath::resolve(path)
        .with_context(|| format!("resolving write target {}", path.display()))?;
    let guard = newt_core::atomic_fs::acquire_lock(&destination.lock_path())
        .with_context(|| format!("locking write target {}", path.display()))?;
    Ok((destination, guard))
}

/// Durably replace `destination` through the shared sibling-stage writer.
fn write_back(destination: &ResolvedPath, text: &str) -> anyhow::Result<()> {
    destination
        .atomic_write(text.as_bytes())
        .with_context(|| format!("writing {}", destination.as_path().display()))
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
        let (destination, _guard) = resolve_and_lock_write_target(&explicit)?;
        let text = read_config_text(destination.as_path())?;
        let updated = Config::with_mcp_server_removed(&text, name)?;
        write_back(&destination, &updated)?;
        return Ok(explicit);
    }
    let dir =
        Config::user_config_dir().ok_or_else(|| anyhow!("cannot resolve ~/.newt (no home dir)"))?;
    for candidate in [dir.join("mcp.toml"), dir.join("config.toml")] {
        let (destination, _guard) = resolve_and_lock_write_target(&candidate)?;
        if let Some(text) = read_optional(destination.as_path())? {
            if config_has_mcp_server(&text, name) {
                let updated = Config::with_mcp_server_removed(&text, name)?;
                write_back(&destination, &updated)?;
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
        // Operator-typed on the CLI — newt-owned, trusted config.
        trust: McpTrust::Trusted,
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
    // Through `markup::table` (#1916). The hand-laid version sized its NAME
    // column with `r.name.len()` — BYTES, which A0 §4.2.13 records as "the
    // wrong metric twice over": a CJK or accented server name sized its column
    // by UTF-8 length and pushed every later column out of line. The one width
    // model measures display cells.
    use newt_core::markup::table::{render_table, Column};
    let columns = [
        Column::new("NAME"),
        Column::new("TRANSPORT"),
        Column::new("ENABLED"),
        Column::new("SOURCE"),
    ];
    let data: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            // The invalid marker stays ATTACHED to the source cell rather than
            // becoming a fifth column: it is a note about that row's origin,
            // and an empty column on every valid row would be worse.
            let note = if row.valid {
                ""
            } else {
                "  (invalid — dropped at discovery; fix or remove it)"
            };
            vec![
                row.name.clone(),
                row.transport.as_str().to_string(),
                if row.enabled { "yes" } else { "no" }.to_string(),
                format!("{}{note}", row.source.label()),
            ]
        })
        .collect();
    write!(out, "{}", render_table(&columns, &data))?;
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
// `newt mcp import` — Claude/Codex config → newt TOML bridge
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportFormat {
    ClaudeJson,
    CodexToml,
    Auto,
}

#[derive(Debug, Clone)]
struct ImportRequest<'a> {
    path: Option<&'a Path>,
    from_claude: bool,
    from_codex: bool,
    name: Option<&'a str>,
    all: bool,
    grant_net: bool,
    force: bool,
    merge: bool,
    config_path: Option<&'a Path>,
    project: bool,
}

/// Resolve exactly one import source. A path is format-auto-detected; the two
/// built-in flags bind both the path and parser so malformed input fails loudly.
fn resolve_import_source(
    path: Option<&Path>,
    from_claude: bool,
    from_codex: bool,
) -> anyhow::Result<(PathBuf, ImportFormat)> {
    if from_claude {
        let home =
            crate::home_dir().ok_or_else(|| anyhow!("cannot resolve $HOME for --from-claude"))?;
        return Ok((home.join(".claude.json"), ImportFormat::ClaudeJson));
    }
    if from_codex {
        let codex_home = std::env::var_os("CODEX_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| crate::home_dir().map(|home| home.join(".codex")))
            .ok_or_else(|| anyhow!("cannot resolve $CODEX_HOME or $HOME for --from-codex"))?;
        return Ok((codex_home.join("config.toml"), ImportFormat::CodexToml));
    }
    match path {
        Some(p) => Ok((p.to_path_buf(), ImportFormat::Auto)),
        None => bail!("provide one MCP config path, --from-claude, or --from-codex"),
    }
}

fn parse_import_entries(text: &str, format: ImportFormat) -> anyhow::Result<McpImportParseReport> {
    let parse_claude = || -> anyhow::Result<McpImportParseReport> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|_| anyhow!("source is not valid Claude JSON"))?;
        Ok(newt_core::mcp::parse_claude_mcp_for_import(&value))
    };
    let parse_codex = || -> anyhow::Result<McpImportParseReport> {
        newt_core::mcp::parse_codex_mcp_toml_for_import(text)
            .map_err(|_| anyhow!("source is not valid Codex TOML"))
    };
    match format {
        ImportFormat::ClaudeJson => parse_claude(),
        ImportFormat::CodexToml => parse_codex(),
        ImportFormat::Auto => match parse_claude() {
            Ok(report) => Ok(report),
            Err(_) => parse_codex()
                .map_err(|_| anyhow!("source is neither valid Claude JSON nor valid Codex TOML")),
        },
    }
}

fn select_import_entries(
    mut report: McpImportParseReport,
    name: Option<&str>,
    all: bool,
    source: &Path,
) -> anyhow::Result<Vec<McpServerEntry>> {
    report.entries.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(name) = name {
        if let Some(entry) = report.entries.into_iter().find(|entry| entry.name == name) {
            return Ok(vec![entry]);
        }
        if let Some(rejected) = report
            .rejected
            .iter()
            .find(|rejected| rejected.name.as_deref() == Some(name))
        {
            bail!(
                "MCP server `{name}` cannot be imported because it has {}; update the source entry before importing",
                rejected.issue
            );
        }
        return Err(anyhow!(
            "no MCP server named `{name}` in {}",
            source.display()
        ));
    }
    if !all {
        bail!("choose exactly one selector: --name <NAME> or --all");
    }
    if !report.rejected.is_empty() {
        let count = report.rejected.len();
        bail!(
            "cannot import all MCP servers: {count} source {} cannot be preserved safely; fix the source or select one valid entry with --name",
            if count == 1 { "entry" } else { "entries" }
        );
    }
    Ok(report.entries)
}

/// Imported names later participate in tool prefixes and token-store lookup.
/// Keep them one portable path component: no separators, traversal aliases,
/// controls, or Windows-reserved punctuation.
fn validate_import_server_name(name: &str) -> anyhow::Result<()> {
    let mut chars = name.chars();
    let safe_shape = name.len() <= 128
        && chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'));
    // Tool calls use `server__tool` on the wire. Reject names that contain that
    // separator either now or after the default hyphen-to-underscore mapping;
    // otherwise `split_once("__")` routes the call to the wrong server.
    let unambiguous_namespace = newt_core::mcp::runtime_server_prefix_is_unambiguous(name, false)
        && newt_core::mcp::runtime_server_prefix_is_unambiguous(name, true);
    // Windows treats these basenames as devices even when an extension follows
    // (`CON.json`, `LPT1.meta.json`, ...), so they are not portable token names.
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let windows_device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|number| {
                matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            });
    // Win32 strips trailing dots when resolving a path component, so a name
    // ending in `.` can alias a different token-store filename.
    let safe = safe_shape && unambiguous_namespace && !windows_device && !name.ends_with('.');
    if !safe {
        bail!(
            "MCP server name is not a portable single-component identifier; rename it before importing"
        );
    }
    Ok(())
}

fn diagnostic_server_name(name: &str) -> &str {
    if validate_import_server_name(name).is_ok() {
        name
    } else {
        "<invalid server name>"
    }
}

/// Runtime's default MCP tool namespace maps hyphens to underscores. Imports
/// must be unique under that effective spelling or two connected servers can
/// advertise the same tool name and first-match routing picks the wrong one.
fn effective_server_namespace(name: &str, sanitize: bool) -> String {
    newt_core::mcp::runtime_server_prefix(name, sanitize)
}

fn reject_namespace_collisions<'a>(
    names: impl IntoIterator<Item = &'a str>,
    sanitize: bool,
) -> anyhow::Result<()> {
    let mut owners = std::collections::BTreeMap::<String, String>::new();
    for name in names {
        let namespace = effective_server_namespace(name, sanitize);
        if let Some(owner) = owners.get(&namespace) {
            if owner != name {
                bail!(
                    "MCP server names `{owner}` and `{name}` share the effective tool namespace `{namespace}`; rename one before importing"
                );
            }
        } else {
            owners.insert(namespace, name.to_string());
        }
    }
    Ok(())
}

fn import_namespace_sanitization(
    target: &Path,
    strong_explicit_target: bool,
) -> anyhow::Result<bool> {
    let user_mcp = Config::user_config_dir().map(|dir| dir.join("mcp.toml"));
    let cfg = if user_mcp.as_deref() == Some(target) && !strong_explicit_target {
        Config::resolve()?
    } else if target.is_file() {
        Config::load(target)?
    } else {
        Config::default()
    };
    Ok(cfg.tui.unwrap_or_default().sanitize_mcp_server_names)
}

fn valid_env_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether a literal consists only of a safe environment reference, with the
/// optional fixed `Bearer ` prefix used by HTTP Authorization headers.
fn safe_env_template(value: &str, allow_bearer: bool) -> bool {
    let trimmed = value.trim();
    let candidate = if allow_bearer {
        trimmed.strip_prefix("Bearer ").unwrap_or(trimmed)
    } else {
        trimmed
    };
    let Some(inner) = candidate
        .strip_prefix("${")
        .and_then(|rest| rest.strip_suffix('}'))
    else {
        return false;
    };
    let env = inner.strip_prefix("env:").unwrap_or(inner);
    valid_env_identifier(env)
}

fn safe_import_value(value: &SecretValue, header: bool) -> bool {
    match value {
        SecretValue::Ref(reference) => {
            reference
                .env
                .as_ref()
                .is_some_and(|name| valid_env_identifier(name))
                && reference.file.is_none()
                && reference.cmd.is_none()
        }
        SecretValue::Literal(value) => {
            if safe_env_template(value, header) {
                return true;
            }
            // Any other interpolation becomes active after explicit import;
            // do not adopt it from a borrowed config source.
            if value.contains("${") {
                return false;
            }
            false
        }
    }
}

/// Validate URL and stdio-argument credential locations. Newt resolves imported
/// references only in MCP headers and child environment values, not URLs or
/// argv, so those locations fail closed. Errors identify only the location;
/// credential values never enter diagnostics.
fn validate_imported_secret_locations(entry: &McpServerEntry) -> anyhow::Result<()> {
    if entry.url.as_deref().is_some_and(|url| url.contains("${")) {
        bail!(
            "MCP server `{}` has an unsupported URL environment reference; use an environment-backed header before importing",
            entry.name
        );
    }
    if entry.args.iter().any(|arg| arg.contains("${")) {
        bail!(
            "MCP server `{}` has an unsupported command-argument environment reference; use the child environment before importing",
            entry.name
        );
    }
    if let Some(url) = entry.url.as_deref() {
        newt_core::mcp::canonical_mcp_http_url(url).map_err(|issue| {
            anyhow!(
                "MCP server `{}` has {issue}; move credentials to environment-backed headers and use a credential-free http(s) URL before importing",
                entry.name
            )
        })?;
    }
    let audit = Config {
        mcp_servers: vec![entry.clone()],
        ..Config::default()
    };
    let redacted_text = audit
        .to_redacted_toml()
        .context("redacting imported MCP configuration")?;
    let redacted = newt_core::mcp::parse_newt_mcp_toml(&redacted_text)
        .into_iter()
        .find(|candidate| candidate.name == entry.name)
        .ok_or_else(|| anyhow!("redacted MCP configuration omitted the selected server"))?;

    if entry.args.len() != redacted.args.len() {
        bail!(
            "MCP server `{}` has an invalid redacted argument list",
            entry.name
        );
    }
    for masked in &redacted.args {
        if !masked.contains(Config::REDACTED) {
            continue;
        }
        bail!(
            "MCP server `{}` has a sensitive command argument; move credentials to the child environment before importing",
            entry.name
        );
    }
    Ok(())
}

/// Remove literal credentials and active file/command references before a
/// borrowed server becomes trusted. Returns redaction-safe field names only.
fn sanitize_imported_secrets(entry: &mut McpServerEntry) -> usize {
    let mut omitted = 0;
    entry.env.retain(|_key, value| {
        let keep = safe_import_value(value, false);
        if !keep {
            omitted += 1;
        }
        keep
    });
    entry.headers.retain(|_key, value| {
        let keep = safe_import_value(value, true);
        if !keep {
            omitted += 1;
        }
        keep
    });
    // The explicit import is the approval boundary. The marker is not
    // serialized, but stamping it here keeps validation and tests honest.
    entry.trust = McpTrust::Trusted;
    omitted
}

fn canonicalize_import_http_url(entry: &mut McpServerEntry) -> anyhow::Result<Option<String>> {
    if entry.transport != TransportKind::Http {
        return Ok(None);
    }
    let raw = entry
        .url
        .as_deref()
        .ok_or_else(|| anyhow!("MCP server `{}` has no HTTP URL", entry.name))?;
    let canonical = newt_core::mcp::canonical_mcp_http_url(raw).map_err(|issue| {
        anyhow!(
            "MCP server `{}` has {issue}; move credentials to environment-backed headers because only credential-free http(s) URLs can be imported",
            entry.name
        )
    })?;
    entry.url = Some(canonical.url);
    Ok(Some(canonical.host))
}

fn imported_http_hosts(entries: &[McpServerEntry]) -> anyhow::Result<Vec<String>> {
    let mut hosts = std::collections::BTreeSet::new();
    for entry in entries {
        if !entry.enabled || entry.transport != TransportKind::Http {
            continue;
        }
        let raw = entry
            .url
            .as_deref()
            .ok_or_else(|| anyhow!("MCP server `{}` has no HTTP URL", entry.name))?;
        let canonical = newt_core::mcp::canonical_mcp_http_url(raw)
            .map_err(|issue| anyhow!("MCP server `{}` has {issue}", entry.name))?;
        hosts.insert(canonical.host);
    }
    Ok(hosts.into_iter().collect())
}

/// Whether an active walked-up project config structurally replaces the base
/// MCP array. In that mode no connector written to the user/explicit base can
/// become effective, regardless of names, so a grant import must fail closed.
fn project_replaces_base_mcp_servers(base: &ResolvedPath) -> anyhow::Result<Option<PathBuf>> {
    let Some(project) = Config::project_config_path() else {
        return Ok(None);
    };
    if ResolvedPath::resolve(&project)? == *base {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&project)
        .with_context(|| format!("reading project config {}", project.display()))?;
    let value: toml::Value = toml::from_str(&text)
        .with_context(|| format!("parsing project config {}", project.display()))?;
    let has_project_mcp_array = value
        .get("mcp_servers")
        .and_then(toml::Value::as_array)
        .is_some();
    let project_strategy = value
        .get("merge")
        .and_then(|merge| merge.get("arrays"))
        .and_then(toml::Value::as_str);
    let base_strategy = base
        .as_path()
        .is_file()
        .then(|| Config::load(base.as_path()))
        .transpose()?
        .and_then(|config| config.merge)
        .map(|merge| merge.arrays);
    let replaces = match project_strategy {
        Some("append") => false,
        Some("replace") => true,
        Some(_) => true,
        None => base_strategy != Some(ArrayMergeStrategy::Append),
    };
    Ok((has_project_mcp_array && replaces).then_some(project))
}

fn project_appends_to_base_mcp_servers(base: &ResolvedPath) -> anyhow::Result<bool> {
    let Some(project) = Config::project_config_path() else {
        return Ok(false);
    };
    if ResolvedPath::resolve(&project)? == *base {
        return Ok(false);
    }
    let text = std::fs::read_to_string(&project)
        .with_context(|| format!("reading project config {}", project.display()))?;
    let value: toml::Value = toml::from_str(&text)
        .with_context(|| format!("parsing project config {}", project.display()))?;
    if value
        .get("mcp_servers")
        .and_then(toml::Value::as_array)
        .is_none()
    {
        return Ok(false);
    }
    let project_strategy = value
        .get("merge")
        .and_then(|merge| merge.get("arrays"))
        .and_then(toml::Value::as_str);
    let base_strategy = base
        .as_path()
        .is_file()
        .then(|| Config::load(base.as_path()))
        .transpose()?
        .and_then(|config| config.merge)
        .map(|merge| merge.arrays);
    Ok(matches!(project_strategy, Some("append"))
        || (project_strategy.is_none() && base_strategy == Some(ArrayMergeStrategy::Append)))
}

fn permission_write_target(mcp_target: &Path, grant_net: bool) -> anyhow::Result<PathBuf> {
    let user_dir =
        Config::user_config_dir().ok_or_else(|| anyhow!("cannot resolve ~/.newt (no home dir)"))?;
    if mcp_target == user_dir.join("mcp.toml") {
        if grant_net {
            if let Some(ambient) = ambient_newt_toml()? {
                bail!(
                    "cannot grant MCP network access while ambient {} is the active base config; run the import outside that directory or select an operator-owned config with --config",
                    ambient.display()
                );
            }
        }
        return Ok(user_dir.join("config.toml"));
    }
    Ok(mcp_target.to_path_buf())
}

/// A network grant must be committed in the same durable replacement as the
/// connector that consumes it. At user scope this selects `config.toml` as the
/// one authoritative document; a strong explicit target (`--config` or
/// `$NEWT_CONFIG`) is already unified and must not be redirected.
/// No-grant imports keep the break-out `mcp.toml` behavior.
fn authoritative_import_target(
    mcp_target: &Path,
    grant_net: bool,
    strong_explicit_target: bool,
) -> anyhow::Result<PathBuf> {
    if grant_net && !strong_explicit_target {
        permission_write_target(mcp_target, true)
    } else {
        Ok(mcp_target.to_path_buf())
    }
}

/// Config sources that can outrank the user-owned `mcp.toml` in at least one
/// normal invocation. The ambient source matters in the current cwd; the user
/// config matters everywhere that ambient source is absent. Checking both keeps
/// an import from becoming silently shadowed as soon as the operator changes
/// directories.
fn outranking_import_sources(
    mcp_target: &Path,
    strong_explicit_target: bool,
) -> anyhow::Result<Vec<(PathBuf, std::collections::BTreeSet<String>)>> {
    if strong_explicit_target {
        return Ok(Vec::new());
    }
    let Some(user_dir) = Config::user_config_dir() else {
        return Ok(Vec::new());
    };
    if mcp_target != user_dir.join("mcp.toml") {
        return Ok(Vec::new());
    }

    let mut sources = Vec::new();

    // Inspect each source locally instead of attributing Config::resolve()'s
    // merged names to its highest-precedence label. Otherwise a project file
    // with no MCP entries can make the user config's own names look project-
    // owned and falsely defeat `--merge` / `--force` on the authoritative file.
    let user_config = user_dir.join("config.toml");
    if user_config.is_file() {
        sources.push((
            user_config.clone(),
            Config::load(&user_config)?
                .mcp_servers
                .into_iter()
                .map(|entry| entry.name)
                .collect(),
        ));
    }
    for source in [ambient_newt_toml()?, Config::project_config_path()]
        .into_iter()
        .flatten()
    {
        if source.is_file() {
            sources.push((
                source.clone(),
                Config::load(&source)?
                    .mcp_servers
                    .into_iter()
                    .map(|entry| entry.name)
                    .collect(),
            ));
        }
    }
    Ok(sources)
}

fn borrowed_runtime_server_names() -> anyhow::Result<std::collections::BTreeSet<String>> {
    let home = crate::home_dir();
    let workspace = std::env::current_dir().context("cannot resolve the current directory")?;
    Ok(
        newt_core::mcp::discover(&[], None, home.as_deref(), &workspace)
            .into_iter()
            .map(|entry| entry.name)
            .collect(),
    )
}

/// Sort physical destination identities before taking any lock. Deduplicating
/// after resolution makes aliases of the same file share one lock generation.
fn sorted_unique_import_targets(mut targets: Vec<ResolvedPath>) -> Vec<ResolvedPath> {
    targets.sort_by(|left, right| left.as_path().cmp(right.as_path()));
    targets.dedup_by(|left, right| left.as_path() == right.as_path());
    targets
}

/// Destinations whose operator-owned state determines a default user-scope
/// import. Lock them before taking any snapshot so a cooperating config writer
/// cannot change precedence or namespace sanitization between validation and
/// the `mcp.toml` commit. Strong explicit targets are self-contained.
fn import_lock_targets(
    target: ResolvedPath,
    breakout: ResolvedPath,
    breakout_path: &Path,
    strong_explicit_target: bool,
    grant_net: bool,
) -> anyhow::Result<Vec<ResolvedPath>> {
    let mut targets = vec![target, breakout];
    let user_mcp = Config::user_config_dir().map(|dir| dir.join("mcp.toml"));
    if !strong_explicit_target && user_mcp.as_deref() == Some(breakout_path) {
        let consulted = [
            Config::user_config_path(),
            ambient_newt_toml()?,
            Config::project_config_path(),
        ];
        for path in consulted.into_iter().flatten() {
            targets.push(
                ResolvedPath::resolve(&path).with_context(|| {
                    format!("resolving consulted MCP config {}", path.display())
                })?,
            );
        }
    }
    if grant_net {
        if let Some(project) = Config::project_config_path() {
            targets.push(ResolvedPath::resolve(&project).with_context(|| {
                format!("resolving consulted project config {}", project.display())
            })?);
        }
    }
    Ok(targets)
}

/// Acquire every consulted target lock in stable physical-path order and retain
/// all guards through commit. This coordinates the authoritative write with the
/// break-out source read without introducing lock-order inversion.
fn acquire_import_target_locks(targets: Vec<ResolvedPath>) -> anyhow::Result<Vec<LockGuard>> {
    let mut guards = Vec::new();
    for destination in sorted_unique_import_targets(targets) {
        let guard =
            newt_core::atomic_fs::acquire_lock(&destination.lock_path()).with_context(|| {
                format!(
                    "locking MCP import target {}",
                    destination.as_path().display()
                )
            })?;
        guards.push(guard);
    }
    Ok(guards)
}

fn import_target_permissions(
    destination: &ResolvedPath,
) -> anyhow::Result<Option<std::fs::Permissions>> {
    let path = destination.as_path();
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata.permissions())),
        Ok(_) => bail!(
            "refusing to replace non-file resolved MCP import target {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("reading metadata for {}", path.display()))
        }
    }
}

/// Replace one config file without exposing a truncated intermediate state:
/// prepare through [`ResolvedPath`], sync, durably replace, then sync the parent.
fn ensure_expected_file_state(
    destination: &ResolvedPath,
    expected: Option<&str>,
) -> anyhow::Result<()> {
    import_target_permissions(destination)?;
    let current = read_optional(destination.as_path())?;
    if current.as_deref() != expected {
        bail!(
            "MCP import conflict at {}; a concurrent edit was preserved",
            destination.as_path().display()
        );
    }
    Ok(())
}

#[cfg(test)]
fn atomic_write_back(
    destination: &ResolvedPath,
    expected: Option<&str>,
    text: &str,
) -> anyhow::Result<()> {
    atomic_write_back_with(
        destination,
        expected,
        text,
        || {},
        |destination, staged| destination.durable_replace(staged),
    )
}

fn atomic_write_back_with(
    destination: &ResolvedPath,
    expected: Option<&str>,
    text: &str,
    after_stage: impl FnOnce(),
    replace: impl FnOnce(&ResolvedPath, &Path) -> Result<(), newt_core::atomic_fs::DurableReplaceError>,
) -> anyhow::Result<()> {
    let permissions = import_target_permissions(destination)?;
    ensure_expected_file_state(destination, expected)?;
    let staged =
        destination.stage_with_permissions(text.as_bytes(), permissions.as_ref(), false)?;
    after_stage();
    // Re-check immediately before replacement. A non-cooperating edit or a
    // newly introduced symlink at a previously absent destination fails closed.
    if let Err(error) = ensure_expected_file_state(destination, expected) {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    if let Err(error) = replace(destination, &staged) {
        let _ = std::fs::remove_file(&staged);
        // A committed error means the destination name already changed and
        // only the parent-directory fsync failed. Never restore the old file:
        // doing so could erase a connector+grant pair that is already visible.
        return Err(error.into());
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn import_process_failpoint(step: &str) {
    const STEP_ENV: &str = "NEWT_TEST_MCP_IMPORT_KILL_AFTER";
    if std::env::var(STEP_ENV).ok().as_deref() != Some(step) {
        return;
    }
    let Some(ready) = std::env::var_os("NEWT_TEST_MCP_IMPORT_READY") else {
        return;
    };
    std::fs::write(ready, step).expect("writing MCP import failpoint readiness marker");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[cfg(not(debug_assertions))]
fn import_process_failpoint(_step: &str) {}

fn atomic_import_commit(
    destination: &ResolvedPath,
    expected: Option<&str>,
    text: &str,
) -> anyhow::Result<()> {
    atomic_write_back_with(
        destination,
        expected,
        text,
        || import_process_failpoint("staged"),
        |destination, staged| {
            destination.durable_replace_with_sync(staged, |path| {
                import_process_failpoint("replaced");
                #[cfg(unix)]
                if let Some(parent) = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    std::fs::File::open(parent)?.sync_all()?;
                }
                #[cfg(windows)]
                let _ = path;
                Ok(())
            })
        },
    )
}

/// Read a selected set of Claude/Codex MCP servers and write trusted newt
/// registrations. Dedup-by-name: a clash errors by default, `--force`
/// overwrites, and `--merge` skips. `--grant-net` commits the connector and its
/// exact host grants together in one authoritative config replacement.
fn cmd_import(request: ImportRequest<'_>, out: &mut dyn Write) -> anyhow::Result<()> {
    if request.project {
        bail!(
            "`newt mcp import --project` is refused: project MCP authority is borrowed and untrusted; import into the user-owned config instead"
        );
    }
    let (source, format) =
        resolve_import_source(request.path, request.from_claude, request.from_codex)?;
    let text = std::fs::read_to_string(&source)
        .with_context(|| format!("reading {}", source.display()))?;
    let imported = parse_import_entries(&text, format)
        .with_context(|| format!("parsing {}", source.display()))?;
    if imported.entries.is_empty() && imported.rejected.is_empty() {
        bail!("no MCP server entries found in {}", source.display());
    }
    let mut imported = select_import_entries(imported, request.name, request.all, &source)?;
    let mut source_omissions = newt_core::mcp::codex_mcp_omitted_field_counts(&text);
    for entry in &mut imported {
        validate_import_server_name(&entry.name)?;
        validate_imported_secret_locations(entry)?;
        // URL validity and canonical representation are import invariants, not
        // side effects of `--grant-net`: an adopted server must be connectable
        // and its persisted URL and permission host must agree byte-for-byte.
        canonicalize_import_http_url(entry)?;
        let omitted = source_omissions.remove(&entry.name).unwrap_or_default()
            + sanitize_imported_secrets(entry);
        if omitted > 0 {
            bail!(
                "MCP server `{}` contains {} literal or active-reference credential field(s) that cannot be imported safely. Replace them with environment references before importing",
                entry.name,
                omitted
            );
        }
    }
    // import is the break-out gesture: create ~/.newt/mcp.toml when at the user
    // level (create=true), unless an explicit target overrides.
    let strong_explicit_target =
        strong_explicit_write_target(request.config_path, request.project)?.is_some();
    let breakout_target = mcp_write_target(request.config_path, request.project, true)?;
    let target =
        authoritative_import_target(&breakout_target, request.grant_net, strong_explicit_target)?;
    let target_destination = ResolvedPath::resolve(&target)
        .with_context(|| format!("resolving MCP import target {}", target.display()))?;
    let breakout_destination = ResolvedPath::resolve(&breakout_target).with_context(|| {
        format!(
            "resolving MCP import break-out target {}",
            breakout_target.display()
        )
    })?;
    let lock_targets = import_lock_targets(
        target_destination.clone(),
        breakout_destination.clone(),
        &breakout_target,
        strong_explicit_target,
        request.grant_net,
    )?;
    let _transaction_locks = acquire_import_target_locks(lock_targets)?;
    target_destination
        .cleanup_abandoned_stages()
        .with_context(|| format!("cleaning abandoned stages for {}", target.display()))?;
    if request.grant_net {
        if let Some(project) = project_replaces_base_mcp_servers(&target_destination)? {
            bail!(
                "cannot grant MCP network access while project config {} replaces the base mcp_servers array; set [merge] arrays = \"append\", remove the project mcp_servers array, or run outside that project",
                project.display()
            );
        }
    }

    let sanitize_names = import_namespace_sanitization(&breakout_target, strong_explicit_target)?;
    reject_namespace_collisions(
        imported.iter().map(|entry| entry.name.as_str()),
        sanitize_names,
    )?;
    import_target_permissions(&target_destination)?;
    let target_original = read_optional(target_destination.as_path())?;
    let mut doc = target_original.clone().unwrap_or_default();
    let existing: std::collections::BTreeSet<String> = newt_core::mcp::parse_newt_mcp_toml(&doc)
        .into_iter()
        .map(|e| e.name)
        .collect();
    let breakout_existing: std::collections::BTreeSet<String> =
        if target_destination == breakout_destination {
            existing.clone()
        } else {
            newt_core::mcp::parse_newt_mcp_toml(
                read_optional(breakout_destination.as_path())?
                    .as_deref()
                    .unwrap_or_default(),
            )
            .into_iter()
            .map(|entry| entry.name)
            .collect()
        };

    let project_appends =
        request.grant_net && project_appends_to_base_mcp_servers(&target_destination)?;
    let project_path = Config::project_config_path()
        .map(|path| ResolvedPath::resolve(&path))
        .transpose()?;
    let lower_precedence_names = if project_appends {
        project_path
            .as_ref()
            .map(ResolvedPath::as_path)
            .map(Config::load)
            .transpose()?
            .map(|config| {
                config
                    .mcp_servers
                    .into_iter()
                    .map(|entry| entry.name)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        std::collections::BTreeSet::new()
    };
    let outranking = outranking_import_sources(&breakout_target, strong_explicit_target)?
        .into_iter()
        .filter(|(path, _)| {
            ResolvedPath::resolve(path)
                .map(|source| source != target_destination)
                .unwrap_or(true)
        })
        .filter(|(path, _)| {
            let is_appended_project = project_appends
                && project_path.as_ref().is_some_and(|project| {
                    ResolvedPath::resolve(path).ok().as_ref() == Some(project)
                });
            !is_appended_project
        })
        .collect::<Vec<_>>();
    let mut existing_names = existing.clone();
    existing_names.extend(breakout_existing.iter().cloned());
    for (_, names) in &outranking {
        existing_names.extend(names.iter().cloned());
    }
    existing_names.extend(borrowed_runtime_server_names()?);
    for entry in &imported {
        for existing_name in &existing_names {
            if existing_name != &entry.name
                && effective_server_namespace(existing_name, sanitize_names)
                    == effective_server_namespace(&entry.name, sanitize_names)
            {
                bail!(
                    "MCP server `{}` conflicts with existing server `{}` under the effective tool namespace; rename one before importing",
                    entry.name,
                    diagnostic_server_name(existing_name)
                );
            }
        }
    }

    let mut added = 0usize;
    let mut overwritten = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut adopted = Vec::new();
    for entry in &imported {
        // A clash with an actually outranking config is always fatal. The
        // authoritative write target itself was filtered above, so its entries
        // retain the normal `--force` / `--merge` behavior.
        if let Some((path, _)) = outranking
            .iter()
            .find(|(_, names)| names.contains(&entry.name))
        {
            bail!(
                "`{}` is already defined in {}, which outranks or can outrank {} — an import here would be ineffective. Remove it there first, or re-run with an explicit --config target",
                entry.name,
                path.display(),
                target.display()
            );
        }
        let exists_in_target = existing.contains(&entry.name);
        let exists_in_breakout = breakout_existing.contains(&entry.name);
        let exists_lower_precedence = lower_precedence_names.contains(&entry.name);
        if exists_in_target || exists_in_breakout || exists_lower_precedence {
            if request.merge {
                skipped.push(entry.name.clone());
                continue;
            }
            if request.force {
                if target_destination != breakout_destination && exists_in_breakout {
                    bail!(
                        "MCP server `{}` exists in {} but --grant-net writes atomically to {}; refusing a cross-file overwrite that would leave a dormant duplicate. Remove the existing server first, then re-run the import",
                        entry.name,
                        breakout_target.display(),
                        target.display()
                    );
                }
                if exists_in_target {
                    doc = Config::with_mcp_server_removed(&doc, &entry.name)?;
                }
                doc = Config::with_mcp_server_added(&doc, entry)?;
                overwritten += 1;
                adopted.push(entry.clone());
                continue;
            }
            let existing_path = if exists_in_target {
                &target
            } else if exists_lower_precedence {
                project_path
                    .as_ref()
                    .map(ResolvedPath::as_path)
                    .unwrap_or(&breakout_target)
            } else {
                &breakout_target
            };
            bail!(
                "MCP server `{}` already exists in {} — use --force to overwrite, \
                 or --merge to skip existing",
                entry.name,
                existing_path.display()
            );
        }
        doc = Config::with_mcp_server_added(&doc, entry)?;
        added += 1;
        adopted.push(entry.clone());
    }
    let hosts = if request.grant_net {
        imported_http_hosts(&adopted)?
    } else {
        Vec::new()
    };
    for host in &hosts {
        doc = Config::with_net_host(&doc, host)?;
    }
    if target_original.as_deref() != Some(doc.as_str()) {
        atomic_import_commit(&target_destination, target_original.as_deref(), &doc)?;
    }

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
    if !hosts.is_empty() {
        writeln!(
            out,
            "Granted exact MCP network host(s) in {}: {}",
            target.display(),
            hosts.join(", ")
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
    // Respect an already-pathed command as-is. Check BOTH separators: a Windows
    // path may use `/` or `\`, and config authored on (or copied from) unix uses
    // `/` — `std::path::MAIN_SEPARATOR` alone (`\` on Windows) would miss it.
    if command.contains(['/', '\\']) {
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
            trust: McpTrust::Trusted,
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

    /// **The byte golden for `newt mcp list` as it ships today** (#1916).
    /// Captured from the shipping renderer — see `models_cmd::d3c`.
    #[test]
    fn the_mcp_listing_is_byte_exact() {
        let rows = merged_rows(
            &[stdio_entry("scrybe", Some("scrybe-mcp-server"))],
            &[stdio_entry("brokenout", Some("bo-mcp"))],
            &[stdio_entry("broken", None)],
            &[],
        );
        let mut out = Vec::new();
        render_rows(&rows, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "| NAME      | TRANSPORT | ENABLED | SOURCE                                                                 |\n",
                "| --------- | --------- | ------- | ---------------------------------------------------------------------- |\n",
                "| scrybe    | stdio     | yes     | newt config                                                            |\n",
                "| brokenout | stdio     | yes     | newt mcp.toml                                                          |\n",
                "| broken    | stdio     | yes     | claude-code (user)  (invalid \u{2014} dropped at discovery; fix or remove it) |\n",
            )
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
            resolve_import_source(Some(Path::new("/tmp/mcp.json")), false, false).unwrap(),
            (PathBuf::from("/tmp/mcp.json"), ImportFormat::Auto)
        );
        // Neither a path nor a built-in source → a loud usage error.
        assert!(resolve_import_source(None, false, false).is_err());
    }

    #[test]
    fn mcp_add_import_parses() {
        let cli = crate::Cli::try_parse_from([
            "newt",
            "mcp",
            "import",
            "/tmp/claude.json",
            "--name",
            "review",
            "--grant-net",
            "--force",
        ])
        .unwrap();
        let Some(crate::Command::Mcp {
            cmd:
                Some(McpCmd::Import {
                    path,
                    from_claude,
                    from_codex,
                    name,
                    all,
                    grant_net,
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
        assert!(!from_codex);
        assert_eq!(name.as_deref(), Some("review"));
        assert!(!all);
        assert!(grant_net);
        assert!(force);
        assert!(!merge);
        assert!(!project);
        // A built-in source makes the path optional, but selection remains
        // explicit so bulk adoption is never accidental.
        assert!(crate::Cli::try_parse_from([
            "newt",
            "mcp",
            "import",
            "--from-claude",
            "--name",
            "review"
        ])
        .is_ok());
        assert!(
            crate::Cli::try_parse_from(["newt", "mcp", "import", "--from-codex", "--all"]).is_ok()
        );
        assert!(crate::Cli::try_parse_from(["newt", "mcp", "import", "--from-claude"]).is_err());
        assert!(crate::Cli::try_parse_from([
            "newt",
            "mcp",
            "import",
            "--from-claude",
            "--from-codex",
            "--all"
        ])
        .is_err());
        // --force and --merge are mutually exclusive.
        assert!(crate::Cli::try_parse_from([
            "newt",
            "mcp",
            "import",
            "/tmp/c.json",
            "--all",
            "--force",
            "--merge"
        ])
        .is_err());
    }

    #[test]
    fn import_sanitizer_preserves_only_safe_secret_references() {
        let mut entry = McpServerEntry {
            name: "review".into(),
            enabled: true,
            transport: TransportKind::Http,
            command: None,
            args: vec![],
            env: BTreeMap::from([
                ("ROOT".into(), SecretValue::literal("/workspace")),
                ("API_TOKEN".into(), SecretValue::literal("plaintext")),
                ("FROM_ENV".into(), SecretValue::literal("${SAFE_TOKEN}")),
                ("ACTIVE".into(), SecretValue::literal("${file:/tmp/token}")),
            ]),
            url: Some("https://broker.example.test/mcp".into()),
            headers: BTreeMap::from([
                (
                    "Authorization".into(),
                    SecretValue::literal("Bearer plaintext"),
                ),
                (
                    "X-Token".into(),
                    SecretValue::literal("Bearer ${env:SAFE_TOKEN}"),
                ),
            ]),
            request_timeout_secs: None,
            trust: McpTrust::Untrusted,
        };

        let omitted = sanitize_imported_secrets(&mut entry);
        assert_eq!(entry.trust, McpTrust::Trusted);
        assert!(!entry.env.contains_key("ROOT"));
        assert!(entry.env.contains_key("FROM_ENV"));
        assert!(!entry.env.contains_key("API_TOKEN"));
        assert!(!entry.env.contains_key("ACTIVE"));
        assert!(!entry.headers.contains_key("Authorization"));
        assert!(entry.headers.contains_key("X-Token"));
        assert_eq!(omitted, 4);
    }

    #[test]
    fn import_validator_rejects_literal_url_and_arg_credentials_without_echoing_them() {
        for mut entry in [
            McpServerEntry {
                name: "userinfo".into(),
                enabled: true,
                transport: TransportKind::Http,
                command: None,
                args: vec![],
                env: BTreeMap::new(),
                url: Some("https://user:do-not-echo@example.test/mcp".into()),
                headers: BTreeMap::new(),
                request_timeout_secs: None,
                trust: McpTrust::Untrusted,
            },
            stdio_entry("argument", Some("mcp-server")),
            stdio_entry("header", Some("mcp-server")),
            stdio_entry("joined-header", Some("mcp-server")),
        ] {
            if entry.name == "argument" {
                entry.args = vec!["--token=do-not-echo".into()];
            } else if entry.name == "header" {
                entry.args = vec!["-H".into(), "X-API-Key: do-not-echo".into()];
            } else if entry.name == "joined-header" {
                entry.args = vec!["-HX-Client-Secret: do-not-echo".into()];
            }
            let error = validate_imported_secret_locations(&entry).unwrap_err();
            assert!(!error.to_string().contains("do-not-echo"), "{error:#}");
        }

        for mut unsafe_ref in [
            stdio_entry("arg-reference", Some("mcp-server")),
            McpServerEntry {
                name: "url-reference".into(),
                enabled: true,
                transport: TransportKind::Http,
                command: None,
                args: vec![],
                env: BTreeMap::new(),
                url: Some("https://${MCP_USERINFO}@example.test/mcp?token=${MCP_TOKEN}".into()),
                headers: BTreeMap::new(),
                request_timeout_secs: None,
                trust: McpTrust::Untrusted,
            },
        ] {
            if unsafe_ref.name == "arg-reference" {
                unsafe_ref.args = vec!["--token".into(), "${MCP_TOKEN}".into()];
            }
            assert!(validate_imported_secret_locations(&unsafe_ref).is_err());
        }

        for args in [
            vec!["--auth=do-not-echo".into()],
            vec!["--oauth2-bearer".into(), "do-not-echo".into()],
            vec!["--cookie".into(), "do-not-echo".into()],
            vec!["--user=operator:do-not-echo".into()],
            vec!["-u".into(), "operator:do-not-echo".into()],
            vec!["-bdo-not-echo".into()],
        ] {
            let mut entry = stdio_entry("argument-alias", Some("mcp-server"));
            entry.args = args;
            let error = validate_imported_secret_locations(&entry).unwrap_err();
            assert!(!error.to_string().contains("do-not-echo"), "{error:#}");
        }
    }

    #[test]
    fn imported_names_are_portable_token_store_components() {
        for invalid in [
            "../other",
            "a/b",
            r"a\b",
            ".",
            "..",
            "bad:name",
            "bad name",
            "CON",
            "lpt1.remote",
            "review.",
            "review__source",
            "review--source",
            "review-_source",
        ] {
            assert!(validate_import_server_name(invalid).is_err(), "{invalid}");
        }
        for valid in ["review", "Case.Sensitive-name", "review_source"] {
            validate_import_server_name(valid).unwrap();
        }
    }

    #[test]
    fn imported_names_are_unique_under_effective_tool_namespacing() {
        reject_namespace_collisions(["review-source", "calendar"], true).unwrap();
        let error =
            reject_namespace_collisions(["review-source", "review_source"], true).unwrap_err();
        assert!(error.to_string().contains("effective tool namespace"));
        reject_namespace_collisions(["review-source", "review_source"], false).unwrap();
    }

    #[test]
    fn exact_http_hosts_are_normalized_and_deduplicated() {
        let entries = [
            McpServerEntry {
                name: "one".into(),
                enabled: true,
                transport: TransportKind::Http,
                command: None,
                args: vec![],
                env: BTreeMap::new(),
                url: Some("https://BROKER.Example.test:8443/mcp".into()),
                headers: BTreeMap::new(),
                request_timeout_secs: None,
                trust: McpTrust::Trusted,
            },
            McpServerEntry {
                name: "two".into(),
                url: Some("https://broker.example.test/other".into()),
                ..stdio_entry("two", None)
            },
            McpServerEntry {
                name: "disabled".into(),
                enabled: false,
                transport: TransportKind::Http,
                command: None,
                args: vec![],
                env: BTreeMap::new(),
                url: Some("https://disabled.example.test/mcp".into()),
                headers: BTreeMap::new(),
                request_timeout_secs: None,
                trust: McpTrust::Trusted,
            },
        ];
        let mut entries = entries;
        entries[1].transport = TransportKind::Http;
        assert_eq!(
            imported_http_hosts(&entries).unwrap(),
            vec!["broker.example.test"]
        );
    }

    #[test]
    fn http_url_is_canonicalized_once_for_persistence_and_grants() {
        let mut entry = McpServerEntry {
            name: "one".into(),
            enabled: true,
            transport: TransportKind::Http,
            command: None,
            args: vec![],
            env: BTreeMap::new(),
            url: Some("https://BÜCHER.example:443/mcp".into()),
            headers: BTreeMap::new(),
            request_timeout_secs: None,
            trust: McpTrust::Trusted,
        };
        let host = canonicalize_import_http_url(&mut entry).unwrap().unwrap();
        assert_eq!(host, "xn--bcher-kva.example");
        assert_eq!(
            entry.url.as_deref(),
            Some("https://xn--bcher-kva.example/mcp")
        );
        assert_eq!(imported_http_hosts(&[entry]).unwrap(), [host]);
    }

    #[test]
    fn import_url_validation_is_independent_of_network_grants_and_rejects_fragments() {
        for url in [
            "ftp://example.test/mcp",
            "https://user:never-echo-this@example.test/mcp",
            "https://example.test/mcp?auth=never-echo-this",
            "https://example.test/mcp#access_token=never-echo-this",
        ] {
            let entry = McpServerEntry {
                name: "review".into(),
                enabled: true,
                transport: TransportKind::Http,
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                url: Some(url.into()),
                headers: BTreeMap::new(),
                request_timeout_secs: None,
                trust: McpTrust::Untrusted,
            };
            let mut entry = entry;
            let error = canonicalize_import_http_url(&mut entry).unwrap_err();
            assert!(!error.to_string().contains("never-echo-this"));
        }
    }

    #[test]
    fn import_selection_never_silently_discards_rejected_entries() {
        let entry = stdio_entry("valid", Some("valid-mcp"));
        let report = McpImportParseReport {
            entries: vec![entry],
            rejected: vec![newt_core::mcp::McpImportRejection {
                name: Some("rejected".into()),
                issue: newt_core::mcp::McpImportIssue::UnknownField,
            }],
        };
        assert!(
            select_import_entries(report.clone(), None, true, Path::new("source.toml"))
                .unwrap_err()
                .to_string()
                .contains("cannot import all")
        );
        assert!(select_import_entries(
            report.clone(),
            Some("rejected"),
            false,
            Path::new("source.toml")
        )
        .unwrap_err()
        .to_string()
        .contains("unsupported fields"));
        assert_eq!(
            select_import_entries(report, Some("valid"), false, Path::new("source.toml")).unwrap()
                [0]
            .name,
            "valid"
        );
    }

    #[test]
    #[ignore = "real filesystem lock acceptance; run in mcp-import-real workflow"]
    #[serial_test::serial(real_fs)]
    fn import_and_ordinary_writers_share_sorted_resolved_locks() {
        let dir = tempfile::tempdir().unwrap();
        let mcp_path = dir.path().join("mcp.toml");
        let mcp = ResolvedPath::resolve(&mcp_path).unwrap();
        let config = ResolvedPath::resolve(&dir.path().join("config.toml")).unwrap();
        let mcp_alias = ResolvedPath::resolve(&dir.path().join(".").join("mcp.toml")).unwrap();

        let targets = sorted_unique_import_targets(vec![mcp, mcp_alias, config]);
        let paths: Vec<&Path> = targets.iter().map(ResolvedPath::as_path).collect();

        assert_eq!(paths.len(), 2);
        assert!(paths[0].ends_with("config.toml"));
        assert!(paths[1].ends_with("mcp.toml"));

        let guards = acquire_import_target_locks(vec![
            targets[1].clone(),
            targets[0].clone(),
            targets[1].clone(),
        ])
        .unwrap();
        assert_eq!(guards.len(), 2);
        assert!(targets.iter().all(|target| target.lock_path().is_file()));
        drop(guards);
        assert!(targets.iter().all(|target| !target.lock_path().exists()));

        let (ordinary_target, _ordinary_guard) = resolve_and_lock_write_target(&mcp_path).unwrap();
        assert_eq!(ordinary_target, targets[1]);
    }

    #[test]
    #[ignore = "real filesystem durability acceptance; run in mcp-import-real workflow"]
    #[serial_test::serial(real_fs)]
    fn post_rename_sync_failure_never_restores_committed_import() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "before").unwrap();
        let destination = ResolvedPath::resolve(&path).unwrap();
        let _guard = newt_core::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();

        let error = atomic_write_back_with(
            &destination,
            Some("before"),
            "after",
            || {},
            |destination, staged| {
                destination.durable_replace_with_sync(staged, |_| {
                    Err(std::io::Error::other("injected parent fsync failure"))
                })
            },
        )
        .unwrap_err();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "after");
        assert!(error.to_string().contains("could not durably sync"));
    }

    #[cfg(unix)]
    /// Grounds the import adapter's use of a once-resolved destination. Even if
    /// an ancestor symlink changes after lock acquisition, commit cannot escape
    /// to the new parent.
    #[test]
    #[ignore = "real filesystem symlink acceptance; run in mcp-import-real workflow"]
    #[serial_test::serial(real_fs)]
    fn import_transaction_stays_bound_when_parent_symlink_is_retargeted() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let parent_link = dir.path().join("active");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        symlink(&first, &parent_link).unwrap();

        let logical = parent_link.join("mcp.toml");
        let destination = ResolvedPath::resolve(&logical).unwrap();
        let _guard = newt_core::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();
        std::fs::remove_file(&parent_link).unwrap();
        symlink(&second, &parent_link).unwrap();
        atomic_write_back(&destination, None, "# imported\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(first.join("mcp.toml")).unwrap(),
            "# imported\n"
        );
        assert!(!second.join("mcp.toml").exists());
        assert_eq!(
            std::fs::canonicalize(&parent_link).unwrap(),
            std::fs::canonicalize(&second).unwrap()
        );
    }

    /// Grounds the pure staged-write tests above against the platform's actual
    /// shared durable replacement behavior. Weekly/release acceptance only.
    #[test]
    #[ignore = "real filesystem acceptance; run in mcp-import-real workflow"]
    #[serial_test::serial(real_fs)]
    fn atomic_import_write_replaces_an_existing_target_without_temp_debris() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("mcp.toml");
        std::fs::write(&target, "before").unwrap();
        let destination = ResolvedPath::resolve(&target).unwrap();
        let _guard = newt_core::atomic_fs::acquire_lock(&destination.lock_path()).unwrap();

        atomic_write_back(&destination, Some("before"), "after").unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "after");
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
    }
}
