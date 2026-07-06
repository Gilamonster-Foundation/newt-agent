//! `newt doctor` — health-check local backends and provider plugins.

use newt_core::dgx::{DgxConfig, EndpointKind};
use newt_core::Config;
use newt_inference::local::LocalOllamaBackend;
use std::path::Path;

pub async fn run(config_path: Option<&Path>) -> anyhow::Result<()> {
    println!("newt doctor — checking backends\n");

    let config = match config_path {
        Some(p) => Config::load(p)?,
        None => Config::resolve()?,
    };

    println!("Configured backends:");
    for backend in &config.backends {
        let status = probe_backend(&backend.endpoint).await;
        println!("  {} ({}) — {status}", backend.name, backend.endpoint);
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
    let engine = config
        .shell
        .as_ref()
        .and_then(|s| s.engine)
        .unwrap_or(newt_core::ShellEngine::SafeSubset);
    println!("  configured engine (L2): {engine}");
    println!("    · safe-subset — refuses $(...)/dynamic constructs (portable default)");
    println!(
        "    · host        — real /bin/sh in the kernel jail (full grammar; --full-access auto-selects)"
    );
    println!(
        "    · brush       — carried bash-in-Rust + L2 interceptor (agent-bridle#20; falls back to host)"
    );
    println!("  override per-run: --shell-engine <safe-subset|host|brush>");
    let (backend, active) = newt_core::ocap_l3_backend();
    println!(
        "  L3 kernel jail (this platform): {backend} — {}",
        if active {
            "available"
        } else {
            "NOT available → a restricted fs grant runs advisory-only (sandbox_kind=None)"
        }
    );
    println!(
        "  agent-bridle attenuates your full ambient authority into structural OCAP grants; \
         --full-access temporarily lifts them."
    );

    println!("\nMCP servers (newt config + Claude Code config):");
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let servers = newt_core::mcp::discover(&config.mcp_servers, home.as_deref(), &workspace);
    if servers.is_empty() {
        println!("  (none discovered)");
    }
    // For stdio servers we actually CONNECT (spawn + initialize + tools/list)
    // so you can see whether each is reachable and which tools it offers.
    for s in &servers {
        match s.transport {
            newt_core::mcp::TransportKind::Stdio => match newt_mcp_client::connect_stdio(s).await {
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
            },
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
