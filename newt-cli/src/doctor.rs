//! `newt doctor` — health-check local backends and provider plugins.

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

    // Also try endpoint discovery.
    println!("\nEndpoint discovery:");
    match LocalOllamaBackend::discover("default").await {
        Ok(backend) => println!("  Ollama: reachable at {}", backend.endpoint()),
        Err(e) => println!("  Ollama: {e}"),
    }

    // Discovered MCP servers — newt's own `[[mcp_servers]]` merged with the
    // servers already configured for Claude Code (~/.claude.json + ./.mcp.json),
    // so you can confirm newt sees the same set without re-configuring anything.
    println!("\nMCP servers (newt config + Claude Code config):");
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let servers = newt_core::mcp::discover(&config.mcp_servers, home.as_deref(), &workspace);
    if servers.is_empty() {
        println!("  (none discovered)");
    }
    for s in &servers {
        let detail = match s.transport {
            newt_core::mcp::TransportKind::Stdio => s.command.clone().unwrap_or_default(),
            newt_core::mcp::TransportKind::Sse | newt_core::mcp::TransportKind::Http => {
                s.url.clone().unwrap_or_default()
            }
        };
        let kind = match s.transport {
            newt_core::mcp::TransportKind::Stdio => "stdio",
            newt_core::mcp::TransportKind::Sse => "sse",
            newt_core::mcp::TransportKind::Http => "http",
        };
        println!("  {} [{kind}] — {detail}", s.name);
    }

    Ok(())
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
