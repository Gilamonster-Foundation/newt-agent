//! `newt mcp probe` — verify-and-enrich a candidate MCP server (#1292).
//!
//! The operator names a target (a stdio command or an `http(s)://` URL) —
//! probing NEVER scans PATH or invents commands. A stdio candidate is spawned
//! under the **confined probe leash** ([`Config::mcp_probe_caveats`] — the
//! same policy `newt doctor` uses; never `Caveats::top()`, #94), through the
//! gate-minted [`newt_mcp_client::connect_stdio`] path (#1256): the child's
//! exec axis is widened by exactly the probed command and nothing else. A URL
//! probe widens the leash's `net` axis by exactly the target host — the typed
//! URL is the consent naming that host — and the dial rides the loopback
//! egress proxy like every other MCP connection (#1267).
//!
//! Transport security (docs/decisions/mcp_transport_security.md): a
//! non-loopback plain-`http://` URL is never dialed silently — it needs
//! `--allow-http`, and still warns. No credential is ever attached by the
//! probe.
//!
//! Consent (the `newt setup` pattern): spawning a candidate IS execution, so
//! the stdio probe confirms `[Y/n]` before the first spawn; `--save` /
//! `--to-catalog` preview the derived entry and confirm before writing.
//! `--yes` skips both; a non-TTY stdin without `--yes` fails closed. Prompts
//! and progress go to **stderr** — stdout carries only the report, so
//! `--json` stays machine-parseable.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use clap::Args;
use newt_core::caveats::Caveats;
use newt_core::mcp::{McpServerEntry, McpTrust, SecretValue, TransportKind};
use newt_core::mcp_probe::{builtin_probe_rules, parse_probe_rules, ProbeRules};
use newt_core::Config;
use newt_mcp_client::{ConnectedServer, NetPosture, SandboxKind, ServerInfo};

#[derive(Args, Debug)]
pub struct ProbeArgs {
    /// The stdio command to spawn, or an http(s):// URL to dial.
    pub target: String,
    /// stdio: one argument for the command (repeatable). Omit to try the
    /// conventional spellings from the probe rules (`stdio`, none, `mcp`,
    /// `serve`), in order.
    #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// stdio: extra environment for the child process (K=V, repeatable).
    #[arg(long = "env", value_name = "K=V")]
    pub env: Vec<String>,
    /// Registration name override (default: the server's self-reported name).
    #[arg(long)]
    pub name: Option<String>,
    /// Per-request timeout override, in seconds.
    #[arg(long, value_name = "N")]
    pub timeout_secs: Option<u64>,
    /// Emit the probe report as JSON on stdout.
    #[arg(long)]
    pub json: bool,
    /// Write the derived `[[mcp_servers]]` entry to the config (the same
    /// write-target rules as `newt mcp add`).
    #[arg(long)]
    pub save: bool,
    /// Upsert the derived entry into the MCP catalog (`mcp-catalog.toml`).
    #[arg(long)]
    pub to_catalog: bool,
    /// Target the project `.newt/` (config and/or catalog) instead of the
    /// user's.
    #[arg(long)]
    pub project: bool,
    /// Skip the execute/write confirmations (non-interactive use).
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Permit dialing a non-loopback plain-http URL. Cleartext is never
    /// dialed silently (mcp_transport_security policy); this flag is the
    /// explicit consent, and a warning still prints.
    #[arg(long)]
    pub allow_http: bool,
}

/// What the positional target names.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeTarget {
    /// A stdio server command to spawn (confined).
    Stdio(String),
    /// An http(s) URL to dial as streamable-HTTP.
    Url(String),
}

/// Classify the target: an `http://` / `https://` prefix (case-insensitive)
/// is a URL; anything else is a stdio command. Never a PATH scan — the
/// string is used exactly as given.
fn classify_target(target: &str) -> ProbeTarget {
    let lower = target.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        ProbeTarget::Url(target.to_string())
    } else {
        ProbeTarget::Stdio(target.to_string())
    }
}

/// `(scheme, host)` for policy decisions — thin wrapper over the client's
/// canonical [`newt_mcp_client::parse_scheme_host`] so the probe's gate and
/// the connection layer can never disagree about what the host is.
fn parse_scheme_host(url: &str) -> (String, String) {
    newt_mcp_client::parse_scheme_host(Some(url))
}

/// Loopback = IP property (or `localhost`), via the client's canonical
/// [`newt_mcp_client::host_is_loopback`] — never a string prefix.
fn host_is_loopback(host: &str) -> bool {
    newt_mcp_client::host_is_loopback(host)
}

/// The transport-security decision for a URL probe.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UrlPolicy {
    /// Encrypted or loopback — dial.
    Proceed,
    /// Non-loopback cleartext, explicitly consented — dial, but warn.
    ProceedWithPlaintextWarning,
    /// Refused; the reason names the policy and the way forward.
    Refuse(String),
}

/// Apply `docs/decisions/mcp_transport_security.md` to a probe URL:
/// `https` and loopback proceed; a non-loopback `http` host needs
/// `--allow-http` (and still warns); anything unparseable or non-http is
/// refused (fail safe).
fn url_probe_policy(url: &str, allow_http: bool) -> UrlPolicy {
    let (scheme, host) = parse_scheme_host(url);
    if host.is_empty() {
        return UrlPolicy::Refuse(format!("cannot parse a host out of `{url}`"));
    }
    match scheme.as_str() {
        "https" => UrlPolicy::Proceed,
        "http" if host_is_loopback(&host) => UrlPolicy::Proceed,
        "http" if allow_http => UrlPolicy::ProceedWithPlaintextWarning,
        "http" => UrlPolicy::Refuse(format!(
            "refusing to dial `{url}`: non-loopback plain-http is cleartext on the wire \
             and is never dialed silently (docs/decisions/mcp_transport_security.md). \
             Use https, or pass --allow-http to consent explicitly."
        )),
        other => UrlPolicy::Refuse(format!(
            "unsupported URL scheme `{other}` — the probe dials http(s) streamable-HTTP only"
        )),
    }
}

/// The argv tails to try: an explicit `--arg` list is exactly one candidate;
/// otherwise the probe rules' ordered list.
fn candidate_arg_lists(explicit: &[String], rules: &ProbeRules) -> Vec<Vec<String>> {
    if explicit.is_empty() {
        rules.arg_candidates.clone()
    } else {
        vec![explicit.to_vec()]
    }
}

/// Sanitize a raw name into a registration-safe one: lowercase; runs of
/// anything outside `[a-z0-9_-]` collapse to a single `-`; trimmed of `-`.
fn sanitize_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_dash = false;
    for ch in raw.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            let is_sep = ch == '-';
            if is_sep {
                pending_dash = true;
            } else {
                if pending_dash && !out.is_empty() {
                    out.push('-');
                }
                pending_dash = false;
                out.push(ch);
            }
        } else {
            pending_dash = true;
        }
    }
    out
}

/// The registration name, by precedence: the operator's `--name` (verbatim —
/// they typed it) > the server's self-reported `serverInfo.name` (sanitized) >
/// the command basename / URL host (sanitized).
fn derive_name(
    name_override: Option<&str>,
    server_info: Option<&ServerInfo>,
    target: &ProbeTarget,
) -> String {
    if let Some(name) = name_override {
        return name.to_string();
    }
    if let Some(reported) = server_info
        .map(|i| sanitize_name(&i.name))
        .filter(|n| !n.is_empty())
    {
        return reported;
    }
    let fallback = match target {
        ProbeTarget::Stdio(command) => {
            let base = Path::new(command)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| command.clone());
            sanitize_name(&base)
        }
        ProbeTarget::Url(url) => {
            let (_scheme, host) = parse_scheme_host(url);
            sanitize_name(&host)
        }
    };
    if fallback.is_empty() {
        "probed-server".to_string()
    } else {
        fallback
    }
}

/// A short human description: the server's `title` when present, else the
/// first line of its `instructions` (truncated to ~120 chars with `…`),
/// else empty.
fn derive_description(server_info: Option<&ServerInfo>, instructions: Option<&str>) -> String {
    // Both sources are server-controlled: clamp either to one bounded,
    // control-character-free line before it reaches a terminal or catalog.
    if let Some(title) = server_info
        .and_then(|i| i.title.as_deref())
        .map(clamp_single_line)
        .filter(|t| !t.is_empty())
    {
        return title;
    }
    instructions.map(clamp_single_line).unwrap_or_default()
}

/// Widen the probe leash's `net` axis by exactly `host` — the URL-probe
/// mirror of the spawn path's exec widening (`spawn_caveats`): the typed URL
/// is operator consent for that one host. `Scope::All` is already
/// unrestricted and stays untouched.
fn widen_net_for_host(caveats: &Caveats, host: &str) -> Caveats {
    use newt_core::caveats::Scope;
    let mut widened = caveats.clone();
    if let Scope::Only(ref mut hosts) = widened.net {
        hosts.extend([host.to_string()]);
    }
    widened
}

/// The HTTP status inside an http-connect failure, if the failure was a
/// non-2xx response — matched by TYPE ([`newt_mcp_client::HttpStatusError`]),
/// never by message text.
fn http_status_of(err: &anyhow::Error) -> Option<u16> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<newt_mcp_client::HttpStatusError>())
        .map(|e| e.status)
}

/// Whether an http connect failure means "authentication required".
fn is_auth_error(err: &anyhow::Error) -> bool {
    matches!(http_status_of(err), Some(401 | 403))
}

/// Whether an http connect failure looks like a legacy SSE-only endpoint
/// (streamable-HTTP POST rejected with 405/404). The legacy SSE transport is
/// not implemented in this build — the probe reports instead of guessing.
fn looks_like_legacy_sse(err: &anyhow::Error) -> bool {
    matches!(http_status_of(err), Some(404 | 405))
}

/// Everything a probe learned, ready to render.
struct ProbeOutcome {
    entry: McpServerEntry,
    description: String,
    tools: Vec<String>,
    sandbox: Option<SandboxKind>,
    net: NetPosture,
    server_info: Option<ServerInfo>,
    /// The endpoint answered HTTP 401/403: reachable, but needs `newt auth`.
    auth_required: bool,
}

/// The human report: identity, tools, confinement posture, and the derived
/// `[[mcp_servers]]` TOML (rendered through the same writer `--save` uses).
fn render_text_report(o: &ProbeOutcome) -> anyhow::Result<String> {
    let mut out = String::new();
    let identity = match &o.server_info {
        Some(si) => {
            // Every field is server-controlled — clamp before the terminal.
            let name = clamp_single_line(&si.name);
            let version = clamp_single_line(&si.version);
            let title = si
                .title
                .as_deref()
                .map(clamp_single_line)
                .unwrap_or_default();
            if title.is_empty() {
                format!("{name} {version}")
            } else {
                format!("{name} {version} ({title})")
            }
        }
        None => "(server did not report an identity)".to_string(),
    };
    if o.auth_required {
        out.push_str(&format!(
            "Probe reached `{}` — authentication required (HTTP 401/403).\n",
            o.entry.url.as_deref().unwrap_or("?")
        ));
    } else {
        out.push_str(&format!("Probe OK: {}\n", o.entry.name));
    }
    out.push_str(&format!("  reported identity : {identity}\n"));
    if !o.description.is_empty() {
        out.push_str(&format!("  description       : {}\n", o.description));
    }
    out.push_str(&format!(
        "  transport         : {}\n",
        o.entry.transport.as_str()
    ));
    // Tool names are server-controlled too.
    let mut tools = o
        .tools
        .iter()
        .map(|t| clamp_single_line(t))
        .collect::<Vec<_>>();
    let extra = tools.len().saturating_sub(8);
    tools.truncate(8);
    let tool_list = if o.tools.is_empty() {
        "(none)".to_string()
    } else if extra > 0 {
        format!("{}, +{extra} more", tools.join(", "))
    } else {
        tools.join(", ")
    };
    out.push_str(&format!(
        "  tools             : {} tool(s): {tool_list}\n",
        o.tools.len()
    ));
    // Honest posture — never over-claimed (#1256).
    let confinement = match o.sandbox {
        Some(SandboxKind::None) => "advisory (no OS sandbox on this host)".to_string(),
        Some(kind) => format!("confined ({kind:?})"),
        None => "remote (no local process to confine)".to_string(),
    };
    out.push_str(&format!("  confinement       : {confinement}\n"));
    let net = match o.net {
        NetPosture::Gated(n) => format!("gated ({n} host{})", if n == 1 { "" } else { "s" }),
        NetPosture::Advisory => "advisory".to_string(),
    };
    out.push_str(&format!("  net egress        : {net}\n"));
    out.push_str("\nDerived registration:\n");
    out.push_str(Config::with_mcp_server_added("", &o.entry)?.trim_end());
    out.push('\n');
    if o.auth_required {
        out.push_str(&format!(
            "\nSave it (--save), then authenticate: `newt auth {}` and re-probe.\n",
            o.entry.name
        ));
    }
    Ok(out)
}

/// The `--json` report.
fn render_json_report(o: &ProbeOutcome) -> anyhow::Result<serde_json::Value> {
    let net = match o.net {
        NetPosture::Gated(n) => serde_json::json!({ "gated_hosts": n }),
        NetPosture::Advisory => serde_json::Value::String("advisory".into()),
    };
    Ok(serde_json::json!({
        "name": o.entry.name,
        "description": o.description,
        "transport": o.entry.transport.as_str(),
        "command": o.entry.command,
        "args": o.entry.args,
        "env": o.entry.env,
        "url": o.entry.url,
        "request_timeout_secs": o.entry.request_timeout_secs,
        "tools": o.tools,
        "sandbox": o.sandbox.map(|k| format!("{k:?}")),
        "net": net,
        "server_info": o.server_info.as_ref().map(|si| serde_json::json!({
            "name": clamp_single_line(&si.name),
            "title": si.title.as_deref().map(clamp_single_line),
            "version": clamp_single_line(&si.version),
        })),
        "auth_required": o.auth_required,
        "toml": Config::with_mcp_server_added("", &o.entry)?,
    }))
}

fn is_yes(input: &str, default: bool) -> bool {
    match input.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    }
}

/// The consent decision from one read of stdin. `bytes_read == 0` is EOF
/// (Ctrl-D) — that is an ABORT, never the default-yes: only an actual
/// newline keypress may take the default (fail closed, like the non-TTY
/// path).
fn consent_given(bytes_read: usize, input: &str) -> bool {
    bytes_read > 0 && is_yes(input, true)
}

/// One bounded, control-character-free line out of server-controlled text —
/// a probed server's title/instructions/version can be multi-KB, multi-line,
/// or ANSI-laced, and reach both the terminal and the catalog.
fn clamp_single_line(s: &str) -> String {
    let first = s.lines().next().unwrap_or("");
    let clean: String = first.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    if clean.chars().count() <= 120 {
        clean.to_string()
    } else {
        clean.chars().take(120).collect::<String>() + "…"
    }
}

/// TTY-gated `[Y/n]` confirmation (the `newt setup` pattern). `--yes` skips;
/// a non-terminal stdin without `--yes` fails closed. Prompts on stderr so
/// stdout stays report-only.
fn confirm_or_bail(question: &str, action: &str, yes: bool) -> anyhow::Result<()> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        bail!("`newt mcp probe` needs confirmation on a terminal for {action}; pass --yes for non-interactive use");
    }
    // Through the seal (#1909). `consent_given` already takes the BYTE COUNT
    // rather than just the string, which is the same EOF-is-not-an-answer
    // distinction `read_line_into` documents — so the resolver is unchanged and
    // only the transport moves.
    //
    // The question moves from stderr to the window, which writes stdout. That
    // is the intentional diff: the seal owns one channel, and a question split
    // across two of them cannot be arbitrated against the ephemeral rows the
    // window just erased. The `is_terminal` bail above still fires first for
    // the piped case, so nothing that used to reach stderr now goes missing.
    let window = newt_core::tty::Terminal::suspend_for_prompt();
    window.ask(&format!("{question} [Y/n] "))?;
    let mut buf = String::new();
    let bytes_read = window.read_line_into(&mut buf)?;
    if consent_given(bytes_read, &buf) {
        Ok(())
    } else {
        bail!("Aborted.");
    }
}

/// Resolve the probe rules: project `.newt/mcp-probe-rules.toml`, else
/// `~/.newt/mcp-probe-rules.toml`, else the bundled defaults. Whole-file
/// replacement (an ordered trial list, not a merge-by-name set); a
/// present-but-malformed file errors loudly with its path.
fn resolve_probe_rules() -> anyhow::Result<ProbeRules> {
    let project = std::env::current_dir()
        .ok()
        .map(|d| d.join(".newt").join("mcp-probe-rules.toml"));
    let user = Config::user_config_dir().map(|d| d.join("mcp-probe-rules.toml"));
    for path in [project, user].into_iter().flatten() {
        if let Some(text) = crate::mcp_cmd::read_optional(&path)? {
            return parse_probe_rules(&text).with_context(|| format!("in {}", path.display()));
        }
    }
    Ok(builtin_probe_rules())
}

fn render_cmdline(command: &str, args: &[String]) -> String {
    if args.is_empty() {
        command.to_string()
    } else {
        format!("{command} {}", args.join(" "))
    }
}

/// The catalog file `--to-catalog` writes: project `.newt/mcp-catalog.toml`
/// under the cwd (the same file `newt mcp install` reads as its project
/// layer), else the user `mcp-catalog.toml`.
fn catalog_write_target(project: bool) -> anyhow::Result<PathBuf> {
    if project {
        Ok(std::env::current_dir()
            .context("cannot resolve the current directory")?
            .join(".newt")
            .join("mcp-catalog.toml"))
    } else {
        Config::user_config_dir()
            .map(|d| d.join("mcp-catalog.toml"))
            .ok_or_else(|| anyhow::anyhow!("cannot resolve ~/.newt (no home dir)"))
    }
}

/// Entry point for `newt mcp probe`.
pub async fn run(args: ProbeArgs, config_path: Option<&Path>) -> anyhow::Result<()> {
    // #1432 — under `--json`, stdout is a PAYLOAD channel, not a human one.
    //
    // This command has always honoured that by hand: every status line is an
    // `eprintln!` and only the report is a `println!`. Hand-discipline holds
    // exactly as long as everyone remembers, and it is about to stop holding —
    // #1312 routes prompts through `PromptWindow::ask`, which hardcodes
    // `io::stdout()`, so the confirm question would land inside the JSON.
    //
    // Take fd 1 away instead. After this, EVERY write to stdout — ours, the
    // arbiter's, anything in the dep tree — goes to stderr, and the report is
    // written through the private handle that is now the only route to the real
    // stdout. That is the same move pi makes (`output-guard.ts` `takeOverStdout`
    // + `writeRawStdout`) and the law codex compiles in (`exec/src/lib.rs`).
    //
    // Only under `--json`: the text path's `println!` IS the human output.
    let payload_stdout = if args.json {
        crate::stdio_guard::redirect_stdout_to_stderr().ok()
    } else {
        None
    };

    let env = crate::mcp_cmd::parse_env_pairs(&args.env)?;
    let cfg = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::resolve()?,
    };
    let workspace = std::env::current_dir().context("cannot resolve the current directory")?;
    // The shared probe leash (#1292 hard rule): exactly doctor's policy —
    // the configured [tui] preset, else ReadOnly; never top().
    let caveats = cfg.mcp_probe_caveats(&workspace);

    let target = classify_target(&args.target);
    let outcome = match &target {
        ProbeTarget::Stdio(command) => probe_stdio(command, &target, &args, env, &caveats).await?,
        ProbeTarget::Url(url) => probe_url(url, &target, &args, &caveats).await?,
    };

    if args.json {
        let report = serde_json::to_string_pretty(&render_json_report(&outcome)?)?;
        match payload_stdout {
            // The guard is installed: fd 1 is stderr now, so the report must go
            // through the private handle or it would join the status stream.
            Some(mut out) => {
                use std::io::Write as _;
                writeln!(out, "{report}").context("writing the --json report")?;
                out.flush().context("flushing the --json report")?;
            }
            // Guard unavailable (non-unix, or dup2 refused): fall back to the
            // hand-discipline that was correct before this change.
            None => println!("{report}"),
        }
    } else {
        println!("{}", render_text_report(&outcome)?);
    }

    if args.save {
        let path = crate::mcp_cmd::write_target(config_path, args.project)?;
        confirm_or_bail(
            &format!(
                "Write [[mcp_servers]] `{}` to {}?",
                outcome.entry.name,
                path.display()
            ),
            "--save",
            args.yes,
        )?;
        let written = crate::mcp_cmd::add_to_config(&outcome.entry, config_path, args.project)
            .with_context(|| {
                format!(
                    "saving `{}` (pass --name <other> to save under a different name)",
                    outcome.entry.name
                )
            })?;
        // Status goes to stderr: stdout is report-only (--json purity).
        eprintln!(
            "Registered MCP server '{}' in {}",
            outcome.entry.name,
            written.display()
        );
        crate::mcp_cmd::print_next_steps(&mut io::stderr())?;
    }
    if args.to_catalog {
        let path = catalog_write_target(args.project)?;
        confirm_or_bail(
            &format!(
                "Upsert catalog entry `{}` into {}?",
                outcome.entry.name,
                path.display()
            ),
            "--to-catalog",
            args.yes,
        )?;
        let text = crate::mcp_cmd::read_config_text(&path)?;
        let updated = newt_core::mcp_catalog::with_catalog_entry(
            &text,
            &outcome.description,
            &outcome.entry,
        )?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, updated).with_context(|| format!("writing {}", path.display()))?;
        eprintln!(
            "Cataloged '{}' in {} — install it with `newt mcp install {}`",
            outcome.entry.name,
            path.display(),
            outcome.entry.name
        );
    }
    Ok(())
}

/// Spawn each candidate under the confined leash until one speaks MCP.
async fn probe_stdio(
    command: &str,
    target: &ProbeTarget,
    args: &ProbeArgs,
    env: BTreeMap<String, SecretValue>,
    caveats: &Caveats,
) -> anyhow::Result<ProbeOutcome> {
    // Rules are only consulted when no --arg pinned the candidate — an
    // explicit --arg probe must not fail on an unrelated rules drop-in.
    let rules = if args.args.is_empty() {
        resolve_probe_rules()?
    } else {
        ProbeRules::default()
    };
    let candidates = candidate_arg_lists(&args.args, &rules);
    if candidates.is_empty() {
        bail!("nothing to try: the probe rules are empty and no --arg was given");
    }
    let spellings: Vec<String> = candidates
        .iter()
        .map(|c| format!("`{}`", render_cmdline(command, c)))
        .collect();
    confirm_or_bail(
        &format!(
            "Probe will EXECUTE (under the confined MCP probe leash): {}.\nProceed?",
            spellings.join(", ")
        ),
        "executing the candidate command",
        args.yes,
    )?;

    // Placeholder identity for spawn errors; replaced by the derived name.
    let placeholder = derive_name(args.name.as_deref(), None, target);
    let mut failures: Vec<(String, String)> = Vec::new();
    for candidate in candidates {
        let entry = McpServerEntry {
            name: placeholder.clone(),
            enabled: true,
            transport: TransportKind::Stdio,
            command: Some(command.to_string()),
            args: candidate.clone(),
            env: env.clone(),
            url: None,
            headers: BTreeMap::new(),
            request_timeout_secs: args.timeout_secs,
            // Operator-typed on the CLI — newt-owned, trusted config.
            trust: McpTrust::Trusted,
        };
        let cmdline = render_cmdline(command, &candidate);
        eprintln!("probing `{cmdline}` …");
        // step-1.1: even an explicit probe goes through the admission gate; an
        // operator-typed entry is trusted, so this admits.
        let admitted = match newt_core::mcp::admit(&entry) {
            Ok(a) => a,
            Err(d) => {
                failures.push((cmdline, format!("not admitted: {d}")));
                continue;
            }
        };
        match newt_mcp_client::connect_stdio(&admitted, caveats).await {
            Ok(server) => {
                return Ok(outcome_from_connection(
                    entry,
                    args.name.as_deref(),
                    target,
                    &server,
                ));
            }
            Err(e) => failures.push((cmdline, format!("{e:#}"))),
        }
    }
    let mut msg = String::from("no candidate spoke MCP:\n");
    for (cmdline, err) in &failures {
        msg.push_str(&format!("  `{cmdline}` → {err}\n"));
    }
    msg.push_str("(pass --arg to pin the exact server arguments)");
    bail!(msg)
}

/// Dial the URL as streamable-HTTP under the host-widened leash.
async fn probe_url(
    url: &str,
    target: &ProbeTarget,
    args: &ProbeArgs,
    caveats: &Caveats,
) -> anyhow::Result<ProbeOutcome> {
    match url_probe_policy(url, args.allow_http) {
        UrlPolicy::Refuse(reason) => bail!(reason),
        UrlPolicy::ProceedWithPlaintextWarning => {
            eprintln!(
                "warning: probing a non-loopback plain-http URL — traffic (and anything the \
                 server echoes) is cleartext on the wire (mcp_transport_security policy)"
            );
        }
        UrlPolicy::Proceed => {}
    }
    let (_scheme, host) = parse_scheme_host(url);
    // The typed URL is the consent naming this one host; the dial itself
    // still rides the egress proxy inside connect_http (#1267).
    let caveats = widen_net_for_host(caveats, &host);
    let entry = McpServerEntry {
        name: derive_name(args.name.as_deref(), None, target),
        enabled: true,
        transport: TransportKind::Http,
        command: None,
        args: vec![],
        env: BTreeMap::new(),
        url: Some(url.to_string()),
        headers: BTreeMap::new(),
        request_timeout_secs: args.timeout_secs,
        // Operator-typed on the CLI — newt-owned, trusted config.
        trust: McpTrust::Trusted,
    };
    eprintln!("probing {url} …");
    // step-1.1: admission gate (operator-typed entry is trusted → admits).
    let admitted = newt_core::mcp::admit(&entry)
        .map_err(|d| anyhow::anyhow!("MCP server not admitted: {d}"))?;
    match newt_mcp_client::connect_http(&admitted, &caveats).await {
        Ok(server) => Ok(outcome_from_connection(
            entry,
            args.name.as_deref(),
            target,
            &server,
        )),
        Err(e) if is_auth_error(&e) => Ok(ProbeOutcome {
            description: String::new(),
            tools: vec![],
            sandbox: None,
            net: NetPosture::Advisory,
            server_info: None,
            auth_required: true,
            entry,
        }),
        Err(e) if looks_like_legacy_sse(&e) => Err(e).context(
            "the endpoint rejected the streamable-HTTP initialize — this may be a legacy \
             SSE-only server; that transport is not implemented in this build",
        ),
        Err(e) => Err(e),
    }
}

/// Fold a successful connection into the outcome, deriving the final name
/// and description from what the server reported about itself.
fn outcome_from_connection(
    mut entry: McpServerEntry,
    name_override: Option<&str>,
    target: &ProbeTarget,
    server: &ConnectedServer,
) -> ProbeOutcome {
    entry.name = derive_name(name_override, server.server_info.as_ref(), target);
    ProbeOutcome {
        description: derive_description(
            server.server_info.as_ref(),
            server.instructions.as_deref(),
        ),
        tools: server.tools.iter().map(|t| t.name.clone()).collect(),
        sandbox: server.sandbox_kind,
        net: server.net_posture,
        server_info: server.server_info.clone(),
        auth_required: false,
        entry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use newt_core::caveats::Scope;

    fn info(name: &str, title: Option<&str>, version: &str) -> ServerInfo {
        ServerInfo {
            name: name.into(),
            title: title.map(str::to_string),
            version: version.into(),
        }
    }

    fn stdio_target() -> ProbeTarget {
        ProbeTarget::Stdio("/opt/bin/foo-mcp".into())
    }

    #[test]
    fn probe_parses_with_repeatable_args_and_flags() {
        let cli = crate::Cli::try_parse_from([
            "newt",
            "mcp",
            "probe",
            "scrybe-mcp-server",
            "--arg",
            "stdio",
            "--env",
            "A=1",
            "--name",
            "scrybe",
            "--timeout-secs",
            "30",
            "--json",
            "--save",
            "--to-catalog",
            "--project",
            "--yes",
        ])
        .unwrap();
        let Some(crate::Command::Mcp {
            cmd: Some(crate::mcp_cmd::McpCmd::Probe(p)),
        }) = cli.command
        else {
            panic!("expected mcp probe");
        };
        assert_eq!(p.target, "scrybe-mcp-server");
        assert_eq!(p.args, vec!["stdio"]);
        assert_eq!(p.env, vec!["A=1"]);
        assert_eq!(p.name.as_deref(), Some("scrybe"));
        assert_eq!(p.timeout_secs, Some(30));
        assert!(p.json && p.save && p.to_catalog && p.project && p.yes);
        assert!(!p.allow_http);
    }

    #[test]
    fn classify_urls_vs_commands() {
        assert_eq!(
            classify_target("https://mcp.example/x"),
            ProbeTarget::Url("https://mcp.example/x".into())
        );
        assert_eq!(
            classify_target("HTTP://x"),
            ProbeTarget::Url("HTTP://x".into())
        );
        assert_eq!(
            classify_target("/usr/local/bin/scrybe-mcp-server"),
            ProbeTarget::Stdio("/usr/local/bin/scrybe-mcp-server".into())
        );
        assert_eq!(
            classify_target("scrybe-mcp-server"),
            ProbeTarget::Stdio("scrybe-mcp-server".into())
        );
    }

    #[test]
    fn scheme_host_parses_ports_userinfo_and_v6() {
        assert_eq!(
            parse_scheme_host("https://MCP.Example:8443/path"),
            ("https".into(), "mcp.example".into())
        );
        assert_eq!(
            parse_scheme_host("http://127.0.0.1:3845/mcp"),
            ("http".into(), "127.0.0.1".into())
        );
        assert_eq!(
            parse_scheme_host("http://[::1]:8080/x"),
            ("http".into(), "::1".into())
        );
        assert_eq!(parse_scheme_host("http://"), ("http".into(), String::new()));
    }

    #[test]
    fn scheme_host_strips_query_fragment_and_userinfo_correctly() {
        // Query/fragment terminate the authority just like '/' — the client's
        // canonical split set. Diverging here corrupted the net-widened host.
        assert_eq!(
            parse_scheme_host("https://mcp.example?key=v"),
            ("https".into(), "mcp.example".into())
        );
        assert_eq!(
            parse_scheme_host("https://mcp.example#frag"),
            ("https".into(), "mcp.example".into())
        );
        // Userinfo is stripped from the AUTHORITY only — an '@' inside the
        // query must not smuggle a fake loopback host past the gate.
        assert_eq!(
            parse_scheme_host("http://evil.example?@127.0.0.1/"),
            ("http".into(), "evil.example".into())
        );
        assert_eq!(
            parse_scheme_host("http://user:pw@real.example:8080/x"),
            ("http".into(), "real.example".into())
        );
    }

    #[test]
    fn loopback_is_an_ip_property_not_a_string_prefix() {
        assert!(host_is_loopback("127.0.0.1"));
        assert!(host_is_loopback("127.9.8.7"));
        assert!(host_is_loopback("::1"));
        assert!(host_is_loopback("localhost"));
        // Valid public DNS names that merely LOOK loopback-ish must not
        // bypass the cleartext gate.
        assert!(!host_is_loopback("127.0.0.1.evil.com"));
        assert!(!host_is_loopback("127.evil.example"));
        assert!(!host_is_loopback("localhost.evil.example"));
        assert!(!host_is_loopback("mcp.example"));
    }

    #[test]
    fn url_policy_closes_the_spoofed_loopback_bypass() {
        // Reproduces the review's live bypasses end-to-end through the policy.
        assert!(matches!(
            url_probe_policy("http://127.0.0.1.evil.com/mcp", false),
            UrlPolicy::Refuse(_)
        ));
        assert!(matches!(
            url_probe_policy("http://evil.example?@127.0.0.1/", false),
            UrlPolicy::Refuse(_)
        ));
    }

    #[test]
    fn url_policy_matrix_follows_transport_security() {
        assert_eq!(
            url_probe_policy("https://mcp.example/x", false),
            UrlPolicy::Proceed
        );
        assert_eq!(
            url_probe_policy("http://127.0.0.1:9/x", false),
            UrlPolicy::Proceed
        );
        assert_eq!(
            url_probe_policy("http://localhost/x", false),
            UrlPolicy::Proceed
        );
        assert_eq!(
            url_probe_policy("http://[::1]:1/x", false),
            UrlPolicy::Proceed
        );
        // Non-loopback cleartext: refused without the explicit flag, and the
        // refusal explains both the policy and the way forward.
        let UrlPolicy::Refuse(reason) = url_probe_policy("http://mcp.example/x", false) else {
            panic!("non-loopback http must be refused without --allow-http");
        };
        assert!(reason.contains("--allow-http"), "{reason}");
        assert!(
            reason.contains("cleartext") || reason.contains("plain-http"),
            "{reason}"
        );
        // With the flag: dialed, never silently.
        assert_eq!(
            url_probe_policy("http://mcp.example/x", true),
            UrlPolicy::ProceedWithPlaintextWarning
        );
        // Non-http schemes and unparseable hosts fail safe.
        assert!(matches!(
            url_probe_policy("ftp://x", false),
            UrlPolicy::Refuse(_)
        ));
        assert!(matches!(
            url_probe_policy("http://", true),
            UrlPolicy::Refuse(_)
        ));
    }

    #[test]
    fn explicit_args_beat_the_rules() {
        let rules = builtin_probe_rules();
        assert_eq!(
            candidate_arg_lists(&["stdio".into(), "-v".into()], &rules),
            vec![vec!["stdio".to_string(), "-v".to_string()]],
            "an explicit --arg list is exactly one candidate"
        );
        let from_rules = candidate_arg_lists(&[], &rules);
        assert_eq!(from_rules.len(), 4);
        assert_eq!(from_rules[0], vec!["stdio".to_string()]);
    }

    #[test]
    fn names_sanitize_and_derive_by_precedence() {
        assert_eq!(
            sanitize_name("Scrybe Markdown Server!"),
            "scrybe-markdown-server"
        );
        assert_eq!(sanitize_name("--weird--"), "weird");
        assert_eq!(sanitize_name("ok_name-1"), "ok_name-1");
        // --name wins, verbatim.
        assert_eq!(
            derive_name(Some("mine"), Some(&info("srv", None, "1")), &stdio_target()),
            "mine"
        );
        // serverInfo.name next, sanitized.
        assert_eq!(
            derive_name(None, Some(&info("Foo Server", None, "1")), &stdio_target()),
            "foo-server"
        );
        // Fallback: command basename …
        assert_eq!(derive_name(None, None, &stdio_target()), "foo-mcp");
        // … or URL host.
        assert_eq!(
            derive_name(
                None,
                None,
                &ProbeTarget::Url("http://127.0.0.1:3845/mcp".into())
            ),
            "127-0-0-1"
        );
    }

    #[test]
    fn description_prefers_title_then_first_instruction_line() {
        assert_eq!(
            derive_description(
                Some(&info("s", Some("Scrybe"), "1")),
                Some("line one\nline two")
            ),
            "Scrybe"
        );
        assert_eq!(
            derive_description(Some(&info("s", None, "1")), Some("  line one  \nline two")),
            "line one"
        );
        assert_eq!(derive_description(None, None), "");
        // A long instructions line is truncated with an ellipsis.
        let long = "x".repeat(400);
        let got = derive_description(None, Some(&long));
        assert!(got.chars().count() <= 121, "truncated: {} chars", got.len());
        assert!(got.ends_with('…'), "{got}");
    }

    #[test]
    fn net_axis_widens_by_exactly_the_probed_host() {
        let read_only = newt_core::ToolPermissions {
            preset: newt_core::PermissionPreset::ReadOnly,
            extra_exec: vec![],
            net: vec![],
            prompt: false,
        }
        .to_caveats("/ws");
        let widened = widen_net_for_host(&read_only, "mcp.example");
        match &widened.net {
            Scope::Only(hosts) => {
                assert_eq!(hosts.len(), 1, "exactly the one host: {hosts:?}");
                assert!(hosts.contains("mcp.example"));
            }
            Scope::All => panic!("must never escalate to All"),
        }
        // An existing allow-list gains the host; All stays All.
        let widened2 = widen_net_for_host(&widened, "other.example");
        match &widened2.net {
            Scope::Only(hosts) => assert_eq!(hosts.len(), 2),
            Scope::All => panic!("must never escalate to All"),
        }
        let mut all = read_only.clone();
        all.net = Scope::All;
        assert!(matches!(widen_net_for_host(&all, "h").net, Scope::All));
    }

    #[test]
    fn auth_and_legacy_sse_failures_are_recognized_by_type() {
        // Built from the client's REAL error type (not a hand-rolled string
        // replica), so a client wording change cannot silently break the
        // probe's detection — the coupling is the type, pinned cross-crate.
        use newt_mcp_client::HttpStatusError;
        let auth = anyhow::Error::new(HttpStatusError::new(401, "Unauthorized", "token missing"))
            .context("initializing MCP server `x`");
        assert!(is_auth_error(&auth));
        let forbidden = anyhow::Error::new(HttpStatusError::new(403, "Forbidden", "nope"));
        assert!(is_auth_error(&forbidden));
        let sse = anyhow::Error::new(HttpStatusError::new(405, "Method Not Allowed", ""))
            .context("initializing MCP server `x`");
        assert!(looks_like_legacy_sse(&sse));
        assert!(!is_auth_error(&sse));
        let server_err = anyhow::Error::new(HttpStatusError::new(500, "Internal Server Error", ""));
        assert!(!is_auth_error(&server_err));
        assert!(!looks_like_legacy_sse(&server_err));
        let plain = anyhow::anyhow!("MCP HTTP request failed");
        assert!(!is_auth_error(&plain));
        assert!(!looks_like_legacy_sse(&plain));
    }

    fn sample_outcome() -> ProbeOutcome {
        ProbeOutcome {
            entry: McpServerEntry {
                name: "scrybe".into(),
                enabled: true,
                transport: TransportKind::Stdio,
                command: Some("scrybe-mcp-server".into()),
                args: vec!["stdio".into()],
                env: BTreeMap::new(),
                url: None,
                headers: BTreeMap::new(),
                request_timeout_secs: None,
                trust: McpTrust::Trusted,
            },
            description: "Scrybe Markdown editor".into(),
            tools: vec!["open".into(), "edit".into()],
            sandbox: Some(SandboxKind::None),
            net: NetPosture::Advisory,
            server_info: Some(info("scrybe", None, "1.2.3")),
            auth_required: false,
        }
    }

    #[test]
    fn text_report_carries_identity_toml_and_posture() {
        let text = render_text_report(&sample_outcome()).unwrap();
        assert!(text.contains("scrybe"), "{text}");
        assert!(text.contains("[[mcp_servers]]"), "{text}");
        assert!(text.contains("command = \"scrybe-mcp-server\""), "{text}");
        assert!(text.contains("2 tool(s)"), "{text}");
        assert!(text.contains("open"), "{text}");
        assert!(
            text.contains("advisory"),
            "honest posture, never over-claimed: {text}"
        );
        assert!(text.contains("1.2.3"), "{text}");
        // The auth-required variant points at `newt auth`.
        let mut auth = sample_outcome();
        auth.auth_required = true;
        let text = render_text_report(&auth).unwrap();
        assert!(text.contains("newt auth"), "{text}");
    }

    #[test]
    fn json_report_is_machine_shaped() {
        let v = render_json_report(&sample_outcome()).unwrap();
        assert_eq!(v["name"], "scrybe");
        assert_eq!(v["transport"], "stdio");
        assert_eq!(v["command"], "scrybe-mcp-server");
        assert_eq!(v["args"][0], "stdio");
        assert_eq!(v["tools"].as_array().unwrap().len(), 2);
        assert_eq!(v["auth_required"], false);
        assert_eq!(v["server_info"]["version"], "1.2.3");
        assert_eq!(v["sandbox"], "None");
        assert_eq!(v["net"], "advisory");
    }

    #[test]
    fn is_yes_matches_the_setup_semantics() {
        assert!(is_yes("", true));
        assert!(!is_yes("", false));
        assert!(is_yes("Y", false));
        assert!(!is_yes("n", true));
    }

    #[test]
    fn consent_requires_input_eof_fails_closed() {
        // Ctrl-D at the prompt reads 0 bytes with an empty buffer — that is
        // NOT a yes. Only an actual newline keypress may take the default.
        assert!(!consent_given(0, ""), "EOF must never proceed to execution");
        assert!(consent_given(1, "\n"), "bare Enter = default yes");
        assert!(consent_given(2, "y\n"));
        assert!(!consent_given(3, "no\n"));
    }

    #[test]
    fn hostile_server_titles_are_clamped_to_one_bounded_clean_line() {
        let hostile = format!("Evil\x1b[2J\x07 Title{}\nsecond line", "x".repeat(400));
        let got = derive_description(Some(&info("s", Some(&hostile), "1")), None);
        assert!(
            !got.contains('\x1b') && !got.contains('\x07'),
            "control chars must not reach the terminal/catalog: {got:?}"
        );
        assert!(!got.contains('\n'), "single line: {got:?}");
        assert!(got.chars().count() <= 121, "bounded: {} chars", got.len());
        assert!(got.starts_with("Evil"), "{got:?}");
        // The identity line clamps too — a hostile title/version never
        // reaches the terminal raw.
        let mut o = sample_outcome();
        o.server_info = Some(info("s", Some(&hostile), "1.0\x1b[31m"));
        let text = render_text_report(&o).unwrap();
        assert!(!text.contains('\x1b'), "{text:?}");
        let json = render_json_report(&o).unwrap();
        let encoded = json.to_string();
        assert!(!encoded.contains("\\u001b"), "{encoded:?}");
        assert!(!encoded.contains("second line"), "{encoded:?}");
        assert!(
            json["server_info"]["title"]
                .as_str()
                .is_some_and(|title| title.chars().count() <= 121),
            "{json}"
        );
    }
}
