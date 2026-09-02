//! `newt doctor` — health-check local backends and provider plugins.

use newt_core::dgx::{DgxConfig, EndpointKind};
use newt_core::ocap::SecurityReport;
use newt_core::Config;
use newt_inference::local::LocalOllamaBackend;
use std::path::Path;

/// Render the achieved-security report as `doctor` posture lines. Pure over the
/// report so it is testable without a live host: one honest line per guarantee,
/// derived from the report (never independent prose), and headed with the
/// platform so an unsupported platform's lines read `unsupported on <plat>`.
fn security_posture_lines(report: &SecurityReport) -> Vec<String> {
    let mut lines = Vec::with_capacity(report.summary_lines().len() + 1);
    lines.push(format!("platform: {}", report.platform));
    lines.extend(report.summary_lines());
    lines
}

pub async fn run(config_path: Option<&Path>, fix: bool) -> anyhow::Result<()> {
    println!("newt doctor — checking backends\n");

    // #1951: a resolution failure is exactly what doctor exists to diagnose —
    // `?` here used to end the whole command with that same failure before a
    // single line of diagnosis printed, which is the defect an operator hit
    // live (a legacy backend drop-in newt could not attribute). Render it as
    // a finding and keep going with everything that does not need a resolved
    // config; only the sections built from `config` below are skipped.
    let config = match config_path {
        Some(p) => Config::load(p),
        None => Config::resolve(),
    };
    let config = match config {
        Ok(c) => Some(c),
        Err(e) => {
            println!("FINDING: config did not resolve — {e}\n");
            None
        }
    };

    // File-level drop-in diagnostics run regardless: they are the ONLY
    // visibility left once resolution has failed, and per-file rather than
    // aggregate, so a second bad file is not hidden behind the first the way
    // `Config::resolve`'s merge (which stops at its first unattributable
    // file) would hide it.
    diagnose_backend_dropins(fix, config.is_some()).await;

    let Some(config) = config else {
        println!(
            "\n(backend/provider/DGX/MCP checks skipped — config did not resolve; \
             see the finding above)"
        );
        return Ok(());
    };

    println!("\nConfigured backends:");
    for backend in &config.backends {
        // #1212: probe by the backend's declared KIND via the BackendApi trait
        // — an openai/vLLM backend answers `/v1/models`, not Ollama's
        // `/api/tags`, and probing the wrong surface reported healthy
        // endpoints as `HTTP 404`. The kind knowledge lives in `api_for`,
        // not here (three-Cs).
        let status = probe_configured_backend(backend).await;
        println!(
            "  {} ({}, {}) — {status}",
            backend.name,
            backend.endpoint,
            backend.kind_label()
        );
        // Credential verdict (encrypted token store, unboxing bundle): where
        // the bearer comes from and whether it is usable right now.
        if let Some(line) = token_status_line(
            backend.api_key_env.as_deref(),
            backend.api_key_file.as_deref(),
        ) {
            println!("    token: {line}");
        }
    }

    println!("\nConfigured providers:");
    if config.providers.is_empty() {
        println!("  (none)");
    }
    for provider in &config.providers {
        let status = probe_provider(&provider.command);
        println!(
            "  {} (command: {}) — {status}",
            provider.name, provider.command
        );
    }

    // DGX nodes from [dgx] config section.
    println!("\nDGX nodes:");
    match &config.dgx {
        None => println!("  (none configured)"),
        Some(dgx) => probe_dgx(dgx).await,
    }

    // Also try endpoint discovery.
    println!("\nEndpoint discovery:");
    match LocalOllamaBackend::discover("default").await {
        Ok(backend) => println!("  Ollama: reachable at {}", backend.endpoint()),
        Err(e) => println!("  Ollama: {e}"),
    }

    // Discovered MCP servers — newt's own `[[mcp_servers]]` merged with the
    // servers already configured for Claude Code (~/.claude.json + ./.mcp.json),
    // so you can confirm newt sees the same set without re-configuring anything.
    // Shell engine + OCAP posture (#868 / #926): which engine parses run_command
    // (L2) and which kernel backend fences it (L3) — separate axes.
    println!("\nShell engine (OCAP):");
    let (backend, l3_active) = newt_core::ocap_l3_backend();
    match config.shell.as_ref().and_then(|s| s.engine) {
        Some(engine) => println!("  configured engine (L2): {engine} (explicit [shell] engine)"),
        None => {
            // #1243 Leg 1: the confined default is L3-gated and resolved
            // per-dispatch — report what THIS host resolves to right now, not a
            // stale hardcoded default. Keyed off the RESOLVED engine (not the
            // raw fence bit) so the reason is consistent on platforms where the
            // brush flip is not enabled (e.g. Windows keeps safe-subset).
            let resolved = newt_core::resolved_confined_default();
            let why = if resolved == newt_core::ShellEngine::Brush {
                "kernel fence enforcing — brush confines dynamic constructs"
            } else {
                "no per-run kernel fence here — safe-subset refuses dynamic constructs"
            };
            println!(
                "  confined default (L2): {resolved} — L3-gated, resolved per run_command ({why})"
            );
        }
    }
    println!("    · safe-subset — refuses $(...)/dynamic constructs (portable default)");
    println!(
        "    · host        — real /bin/sh in the kernel jail (full grammar; --full-access auto-selects)"
    );
    println!(
        "    · brush       — carried bash-in-Rust + L2 interceptor (cross-platform; confines restricted exec too; Windows full-access default)"
    );
    println!("  override per-run: --shell-engine <safe-subset|host|brush>");
    println!(
        "  L3 kernel jail (this platform): {backend} — {}",
        if l3_active {
            "available"
        } else {
            "NOT available → a restricted fs grant runs advisory-only (sandbox_kind=None)"
        }
    );
    println!(
        "  agent-bridle attenuates your full ambient authority into structural OCAP grants; \
         --full-access temporarily lifts them."
    );

    // Achieved OCAP posture, per guarantee (#11 / #12). Every line is DERIVED
    // from the same `verify_*` invariants the capability gates enforce (via
    // `SecurityReport::current()`), never hand-written prose — so this diagnostic
    // cannot drift from what is actually enforced, and an unsupported platform
    // reports `unsupported` rather than a Linux-equivalent claim.
    println!("\nAchieved OCAP posture (per guarantee):");
    for line in security_posture_lines(&newt_core::ocap::SecurityReport::current()) {
        println!("  {line}");
    }

    println!("\nMCP servers (newt config + Claude Code config):");
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mcp_toml = newt_core::Config::user_config_dir().map(|d| d.join("mcp.toml"));
    let servers = newt_core::mcp::discover(
        &config.mcp_servers,
        mcp_toml.as_deref(),
        home.as_deref(),
        &workspace,
    );
    if servers.is_empty() {
        println!("  (none discovered)");
    }
    // The leash `doctor` spawns stdio servers under — the SAME confinement the
    // live session applies, derived from the operator's configured preset (else a
    // read-only default), NEVER `Caveats::top()` (#94: no top() in a dispatch
    // path). So the diagnostic shows each server under its real confinement.
    // Shared with `newt mcp probe` (#1292) via Config::mcp_probe_caveats.
    let mcp_caveats = config.mcp_probe_caveats(&workspace);
    // For stdio servers we actually CONNECT (spawn + initialize + tools/list)
    // so you can see whether each is reachable and which tools it offers.
    for s in &servers {
        match s.transport {
            newt_core::mcp::TransportKind::Stdio => {
                // step-1.1: admission gate — doctor does not spawn an untrusted
                // or disabled server; it reports the refusal instead.
                let admitted = match newt_core::mcp::admit(s) {
                    Ok(a) => a,
                    Err(denied) => {
                        println!("  · {} — not admitted: {denied}", s.name);
                        continue;
                    }
                };
                match newt_mcp_client::connect_stdio(&admitted, &mcp_caveats).await {
                    Ok(connected) => {
                        let names: Vec<&str> =
                            connected.tools.iter().map(|t| t.name.as_str()).collect();
                        let list = if names.is_empty() {
                            "(none)".to_string()
                        } else {
                            names.join(", ")
                        };
                        println!("  {} [stdio] — OK, {} tool(s): {list}", s.name, names.len());
                    }
                    Err(e) => println!("  {} [stdio] — ERROR: {e}", s.name),
                }
            }
            newt_core::mcp::TransportKind::Sse | newt_core::mcp::TransportKind::Http => {
                let kind = if matches!(s.transport, newt_core::mcp::TransportKind::Sse) {
                    "sse"
                } else {
                    "http"
                };
                let url = s.url.clone().unwrap_or_default();
                println!(
                    "  {} [{kind}] — {url} (skipped: only stdio is supported in this build)",
                    s.name
                );
            }
        }
    }

    Ok(())
}

/// Every `<dir>/backends/` directory [`Config::resolve`]'s merge reads, home
/// before project (the same precedence), returned once each even if the
/// project config happens to resolve to the same directory as home.
fn backend_dropin_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = Config::user_config_dir() {
        dirs.push(dir.join("backends"));
    }
    if let Some(proj) = Config::project_config_path() {
        if let Some(parent) = proj.parent() {
            let dir = parent.join("backends");
            if !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// `newt doctor`'s file-level backend drop-in diagnostics (#1951): parseable,
/// attributable (operator config vs probe residue), and shadowed — the three
/// questions [`Config::resolve`]'s merge answers only implicitly, by either
/// succeeding or aborting on the first file it cannot place. Doctor answers
/// them explicitly, per file, so one unattributable drop-in does not hide a
/// problem in a sibling the way the merge's early return would, and so this
/// still runs when resolution as a whole failed — which is exactly when it
/// is needed most (see the finding printed above this in [`run`]).
///
/// `config_resolved` gates the live endpoint probe: a resolved config already
/// probes every merged backend in "Configured backends:" below, so probing
/// again here would just repeat that output. When resolution failed, that
/// section never runs at all, and this is the only liveness check an
/// operator gets.
async fn diagnose_backend_dropins(fix: bool, config_resolved: bool) {
    println!("Backend drop-ins:");
    let mut seen_stems: std::collections::HashMap<String, std::path::PathBuf> =
        std::collections::HashMap::new();
    let mut any = false;
    for dir in backend_dropin_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        paths.sort();
        for path in paths {
            any = true;
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if let Some(shadowed) = seen_stems.get(stem) {
                    println!(
                        "  {} — SHADOWED by {} (same name, lower precedence; never read)",
                        shadowed.display(),
                        path.display()
                    );
                } else {
                    seen_stems.insert(stem.to_string(), path.clone());
                }
            }

            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    println!("  {} — FINDING: cannot read ({e})", path.display());
                    continue;
                }
            };
            match newt_core::classify_backend_dropin(&text) {
                Ok(ownership) => {
                    let label = match ownership {
                        newt_core::DropinOwnership::Operator => "operator",
                        newt_core::DropinOwnership::Probe => "probe",
                        _ => "unknown ownership (newer variant than this build knows)",
                    };
                    println!("  {} — OK ({label})", path.display());
                    if !config_resolved {
                        if let Ok(backend) = toml::from_str::<newt_core::BackendConfig>(&text) {
                            if !backend.endpoint.is_empty()
                                && backend.kind != Some(newt_core::BackendKind::Embedded)
                            {
                                let status = probe_configured_backend(&backend).await;
                                println!("    live: {status}");
                            }
                        }
                    }
                }
                Err(reason) => {
                    println!("  {} — FINDING: {reason}", path.display());
                    if fix {
                        repair_ambiguous_dropin(&path, &text).await;
                    }
                }
            }
        }
    }
    if !any {
        println!("  (none)");
    }
}

/// Offer the interactive repair for a drop-in doctor could not attribute:
/// claim it as operator configuration, or discard it as probe residue —
/// mirroring `newt dgx adopt`'s three-way drift menu (the one existing
/// non-TUI consumer of [`newt_core::InteractionDefinition`]) rather than
/// inventing a second convention. Checks `is_tty` itself, before requesting
/// the terminal, the same way that menu does — a piped or headless run
/// refuses to guess (C0b) and reports only, exactly as without `--fix`.
async fn repair_ambiguous_dropin(path: &Path, text: &str) {
    use std::io::IsTerminal as _;
    if !std::io::stdin().is_terminal() {
        println!("    --fix needs a terminal to choose — refusing to guess, nothing changed");
        return;
    }
    let menu = newt_core::interaction_form::menu(
        format!("{}: operator record or probe residue?", path.display()),
        "carries the old newt-adopt probe marker plus operator-looking fields — \
         ambiguous, so nothing changes until you choose",
        &[
            (
                "o",
                "claim as operator config (tag record = \"operator_v1\", keep every field)",
            ),
            (
                "d",
                "discard as probe residue (delete the file; newt re-probes on next use)",
            ),
        ],
    );
    let window = newt_core::tty::Terminal::suspend_for_prompt(
        newt_core::tty::TerminalTaker::PlainCliConfirm,
    );
    let picked = newt_core::interaction_terminal::resolve_on_terminal(&window, &menu);
    match picked.as_ref().map(|id| id.as_str()) {
        Some("o") => match newt_core::claim_backend_dropin_as_operator(text) {
            Ok(claimed) => {
                match newt_core::atomic_fs::ResolvedPath::resolve(path)
                    .and_then(|dest| dest.atomic_write(claimed.as_bytes()))
                {
                    Ok(()) => println!("    fixed: claimed as operator configuration"),
                    Err(e) => println!("    fix failed: {e:#}"),
                }
            }
            Err(e) => println!("    fix failed: {e}"),
        },
        Some("d") => match std::fs::remove_file(path) {
            Ok(()) => println!("    fixed: deleted (probe residue) — newt re-probes on next use"),
            Err(e) => println!("    fix failed: {e}"),
        },
        _ => println!("    no repair chosen — file left as-is"),
    }
}

async fn probe_dgx(dgx: &DgxConfig) {
    let active_node_name = dgx.active_node.as_deref();
    let active_endpoint = dgx.active_endpoint;
    let active_model = dgx.active_model.as_deref().unwrap_or("(none)");

    if dgx.nodes.is_empty() {
        println!("  (no nodes — using env overrides only)");
    }

    for node in &dgx.nodes {
        let is_active_node = active_node_name.map_or(dgx.nodes.len() == 1, |n| n == node.name);
        let node_marker = if is_active_node { " [active node]" } else { "" };
        println!("  {}{node_marker}", node.name);

        for kind in EndpointKind::ALL {
            let Some(url) = node.endpoint(kind) else {
                continue;
            };
            let is_active_ep = is_active_node && kind == active_endpoint;
            let active_marker = if is_active_ep { " *" } else { "" };
            let status = if kind.is_openai_compatible() {
                probe_vllm(url).await
            } else {
                probe_backend(url).await
            };
            println!("    {kind} ({url}){active_marker} — {status}");
        }
    }

    // Resolve and show the active endpoint URL (may come from env vars too).
    match dgx.resolve_endpoint() {
        Ok(url) => println!("  Active: {active_model} @ {active_endpoint} → {url}"),
        Err(e) => println!("  Active endpoint: unresolved ({e})"),
    }
}

/// Probe a `[[backends]]` entry the way a session would reach it (#1212):
/// route by `kind` through [`newt_core::backend_probe::api_for`] — the same
/// list-models call session-start adoption makes, with the same auth. When
/// `kind` is unset, race protocols via [`newt_core::backend_probe::detect_endpoint`].
/// An `embedded` backend has no endpoint to probe; report it as in-process.
async fn probe_configured_backend(backend: &newt_core::config::BackendConfig) -> String {
    if backend.kind == Some(newt_core::config::BackendKind::Embedded) {
        return "in-process (embedded — no endpoint to probe)".to_string();
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    let api_key = backend.resolve_api_key();
    if backend.needs_kind_probe() {
        return match newt_core::backend_probe::detect_endpoint(
            &client,
            &backend.endpoint,
            api_key.as_deref(),
        )
        .await
        {
            Ok(probe) => format!(
                "OK (detected {}, {} model(s) served)",
                probe.kind.label(),
                probe.models.len()
            ),
            Err(e) if e.to_string().starts_with("HTTP ") => e.to_string(),
            Err(e) => format!("unreachable: {e}"),
        };
    }
    let kind = backend.kind.expect("needs_kind_probe was false");
    match newt_core::backend_probe::api_for(kind)
        .list_models(&client, &backend.endpoint, api_key.as_deref())
        .await
    {
        Ok(models) => format!("OK ({} model(s) served)", models.len()),
        // The fetchers bail with `HTTP <status>` for a reachable-but-erroring
        // endpoint — keep that distinct from a connection failure.
        Err(e) if e.to_string().starts_with("HTTP ") => e.to_string(),
        Err(e) => format!("unreachable: {e}"),
    }
}

/// One human line for a backend's credential state, `None` when no
/// credential is configured at all (most local backends — no noise).
fn token_status_line(api_key_env: Option<&str>, api_key_file: Option<&str>) -> Option<String> {
    use newt_core::secrets::TokenStatus;
    match newt_core::secrets::token_status(api_key_env, api_key_file) {
        TokenStatus::Unset => None,
        TokenStatus::FromEnv { var } => Some(format!("from env ${var}")),
        TokenStatus::PlaintextFile { path } => Some(format!(
            "plaintext file {} (re-run `newt setup` to store it encrypted)",
            path.display()
        )),
        TokenStatus::EncryptedUnlocked { path } => {
            Some(format!("encrypted (unlocked) — {}", path.display()))
        }
        TokenStatus::EncryptedLocked { path, reason } => Some(format!(
            "encrypted (LOCKED) — {} · {reason}",
            path.display()
        )),
        TokenStatus::MissingFile { path } => Some(format!("file missing — {}", path.display())),
    }
}

async fn probe_vllm(endpoint: &str) -> String {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    let url = format!("{}/v1/models", endpoint.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => "OK".to_string(),
        Ok(resp) => format!("HTTP {}", resp.status()),
        Err(e) => format!("unreachable: {e}"),
    }
}

async fn probe_backend(endpoint: &str) -> String {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => "OK".to_string(),
        Ok(resp) => format!("HTTP {}", resp.status()),
        Err(e) => format!("unreachable: {e}"),
    }
}

fn probe_provider(command: &str) -> &'static str {
    let status = std::process::Command::new(command)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => "found on PATH",
        Ok(_) => "found but exited with error",
        Err(_) => "not found on PATH",
    }
}

/// `newt doctor --sign-ocap` (#1207): the blessing ceremony for
/// `~/.newt/ocap/approve.toml`. A PRESENT human running this command is
/// explicitly vouching for the file as it stands — every entry is re-signed
/// with the operator's root key (the same newt-identity root the session's
/// operating authority is minted from), which is the ONLY way hand-edited
/// entries become valid durable grants. High-danger targets (interpreters,
/// broad fs roots — the production danger table's judgment) are REFUSED a
/// signature, per the contract's mandatory `validate_approve` check, and are
/// reported so the operator can move them to `passkey.toml` or delete them.
///
/// Returns the process exit code: 0 = blessed (or nothing to sign); 2 = one
/// or more entries were refused (the file was still written — valid entries
/// are blessed, refused ones stay unsigned and will drop at load,
/// fail-closed). Errors bubble as `Err` (exit 1).
pub fn sign_ocap() -> anyhow::Result<i32> {
    use newt_core::ocap_store::{self, PolicyFile, Verdict};

    let Some(config_path) = newt_core::Config::user_config_path() else {
        anyhow::bail!("cannot resolve the user config directory (~/.newt)");
    };
    let approve_path = config_path
        .with_file_name("ocap")
        .join(Verdict::Approve.filename());
    let text = match std::fs::read_to_string(&approve_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("nothing to sign: {} does not exist", approve_path.display());
            return Ok(0);
        }
        Err(e) => anyhow::bail!("cannot read {}: {e}", approve_path.display()),
    };
    let mut file = PolicyFile::parse(&text).map_err(|e| anyhow::anyhow!(e))?;

    let key_path = newt_identity::default_key_path()?;
    let user = newt_identity::load_or_generate(&key_path)?;

    let (signed, refused) = ocap_store::sign_approves(
        &mut file,
        newt_tui::ocap_high_danger_predicate(),
        |payload| user.sign(payload).to_bytes(),
    );
    std::fs::write(
        &approve_path,
        file.to_toml().map_err(|e| anyhow::anyhow!(e))?,
    )
    .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", approve_path.display()))?;

    println!(
        "blessed {}: {signed} entr{} signed with the root key ({})",
        approve_path.display(),
        if signed == 1 { "y" } else { "ies" },
        key_path.display()
    );
    for r in &refused {
        println!("  REFUSED: {r}");
    }

    // Prove the result the way the session will see it: reload through the
    // same verify-at-load path and report the live durable grants.
    let (set, warnings) = ocap_store::load_store(&config_path, Some(user.public().as_bytes()));
    for w in &warnings {
        println!("  load warning: {w}");
    }
    let live = set
        .files
        .get(&Verdict::Approve)
        .map(|f| f.exec.len() + f.fs.len() + f.net.len())
        .unwrap_or(0);
    println!("verified: {live} durable grant(s) live at load");

    Ok(if refused.is_empty() { 0 } else { 2 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use newt_core::ocap::{Guarantee, RuntimeEvidence, LINUX_CEILING, MACOS_CEILING};

    #[test]
    fn posture_lines_are_derived_from_the_report_not_prose() {
        // #11: doctor renders exactly the report's guarantees, one line each,
        // plus a platform header — the diagnostic cannot drift from enforcement.
        let report = SecurityReport::from_parts(&LINUX_CEILING, &RuntimeEvidence::current());
        let lines = security_posture_lines(&report);
        assert_eq!(lines.len(), Guarantee::ALL.len() + 1);
        assert_eq!(lines[0], "platform: linux");
        // #1711: disclosure filtering is a per-THREAD probe, not a build-wide
        // constant. `doctor` runs with no session filter installed, so the
        // honest render is OPEN — and, like the b1 rows below, it names its
        // deviation rather than silently claiming the guarantee.
        assert!(lines
            .iter()
            .any(|l| l == "disclosure-filtering: OPEN (disclosure-gate-live-path)"));
        // …and the render still tracks the verifier: with the backstop actually
        // installed on this thread, the same code path says enforced. Without
        // this half the assertion above would pass against a render that had
        // simply hard-coded the row.
        {
            let mut filter = newt_core::ocap::DisclosureFilter::new();
            filter.register("sk-probe-9f3a2b7c1d4e6a8b");
            let _guard = newt_core::ocap::scoped_session_disclosure(filter);
            let live = SecurityReport::from_parts(&LINUX_CEILING, &RuntimeEvidence::current());
            assert!(security_posture_lines(&live)
                .iter()
                .any(|l| l == "disclosure-filtering: enforced"));
        }
        // b1-gated guarantees are honestly OPEN, not silently claimed.
        assert!(lines
            .iter()
            .any(|l| l.contains("network-confinement: OPEN (b1-os-isolation)")));
    }

    #[test]
    fn unsupported_platform_never_renders_a_linux_equivalent_claim() {
        // #12: even if every runtime verifier were Verified, a macOS render
        // reports kernel-backed guarantees as `unsupported on macos`.
        let all_verified = {
            let v = || newt_core::ocap::Verification::Verified {
                evidence: "synthetic".into(),
            };
            RuntimeEvidence {
                b1: v(),
                disclosure: v(),
                fs_object_bound: v(),
                constrained_executor: v(),
                fail_closed: v(),
            }
        };
        let report = SecurityReport::from_parts(&MACOS_CEILING, &all_verified);
        let lines = security_posture_lines(&report);
        assert_eq!(lines[0], "platform: macos");
        assert!(lines
            .iter()
            .any(|l| l == "fs-confinement: unsupported on macos"));
        assert!(lines
            .iter()
            .any(|l| l == "process-confinement: unsupported on macos"));
        // No line may claim a kernel guarantee is "enforced" on macOS.
        assert!(!lines.iter().any(|l| l == "fs-confinement: enforced"));
    }
}
