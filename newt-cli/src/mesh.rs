//! `newt mesh` subcommands — only compiled with `--features mesh`.
//!
//! Two operations:
//!
//! - `newt mesh announce` — bind a responder service on the LAN that
//!   answers `InferenceRequest`s using the local Ollama backend.
//! - `newt mesh ask <peer_fp> <prompt>` — resolve a peer by
//!   fingerprint (full or short prefix) via mDNS, then send it an
//!   `InferenceRequest` and print the reply.
//!
//! The trust root is loaded from `~/.agent-mesh/user.key` by default;
//! both subcommands accept a `--user-key` override.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_mesh_core::{AgentKey, AgentMetadata, Fingerprint, UserKey};
use agent_mesh_discovery::{Browser, BrowserEvent};
use anyhow::{Context, Result};
use newt_inference::backend::InferenceBackend;
use newt_inference::local::LocalOllamaBackend;
use newt_mesh::{InferenceRequest, MeshAsker, NewtMeshService, CAPABILITY_TAG};

/// Default model when the user doesn't override it via env or flag.
const DEFAULT_MODEL: &str = "llama3.1:8b";

/// Run the `announce` subcommand.
///
/// Binds a [`NewtMeshService`] backed by the local Ollama instance
/// (discovered the same way `newt worker` does), then blocks until
/// the user hits Ctrl-C.
pub async fn announce(
    user_key_path: Option<PathBuf>,
    extra_capabilities: Vec<String>,
    port: u16,
    role: String,
    model: Option<String>,
) -> Result<()> {
    let user = load_user_key(user_key_path)?;
    let model = model.unwrap_or_else(|| DEFAULT_MODEL.to_string());

    let backend = LocalOllamaBackend::discover(&model)
        .await
        .with_context(|| format!("discover local Ollama for model {model}"))?;
    let backend: Arc<dyn InferenceBackend> = Arc::new(backend);

    let mut caps = vec![CAPABILITY_TAG.to_string()];
    caps.push(format!("model={}", backend.model_id()));
    caps.extend(extra_capabilities);

    let agent = issue_agent(&user, &role, caps.clone());

    let service = NewtMeshService::bind(&user, agent, backend, port).await?;

    println!("newt mesh service running");
    println!("  agent_fp:  {}", service.agent_fingerprint().hex());
    println!("  short:     {}", service.agent_fingerprint().short());
    println!("  user_fp:   {}", service.user_fingerprint().hex());
    println!("  port:      {}", service.local_port());
    println!("  backend:   {}", service.backend_name());
    println!("  model:     {}", service.backend_model());
    println!("  caps:      {}", caps.join(","));
    println!("  ctrl-c to stop");

    tokio::signal::ctrl_c().await.context("ctrl-c handler")?;
    println!("\nshutting down...");
    service.close().await?;
    Ok(())
}

/// Run the `ask` subcommand.
pub async fn ask(
    user_key_path: Option<PathBuf>,
    peer_fp_str: String,
    prompt: String,
    tier: Option<String>,
    model: Option<String>,
    max_tokens: Option<u32>,
    timeout: String,
) -> Result<()> {
    let user = load_user_key(user_key_path)?;
    let agent = issue_agent(&user, "newt-asker", vec!["newt-asker".to_string()]);
    let asker = MeshAsker::bind(&user, agent).await?;

    let lookup_deadline = Duration::from_secs(5);
    let peer_fp = resolve_peer_fp(&peer_fp_str, user.fingerprint(), lookup_deadline).await?;

    let parsed_tier = tier.as_deref().map(parse_tier).transpose()?;
    let req = InferenceRequest {
        prompt,
        tier: parsed_tier,
        model,
        max_tokens,
    };

    let timeout = parse_duration(&timeout)?;
    println!("asking peer {} ...", peer_fp.short());
    let reply = asker.ask(peer_fp, req, timeout).await?;

    if reply.is_error() {
        println!(
            "responder error from model {}: {}",
            reply.model_id,
            reply.error.unwrap_or_default()
        );
        asker.close().await?;
        anyhow::bail!("responder returned an error");
    }

    println!("reply from {}:\n{}", reply.model_id, reply.content);
    asker.close().await?;
    Ok(())
}

/// Load the user key, defaulting to `~/.agent-mesh/user.key` if no
/// override is supplied.
fn load_user_key(path: Option<PathBuf>) -> Result<UserKey> {
    let p = path.unwrap_or_else(default_user_key_path);
    UserKey::load(&p).with_context(|| format!("load user key {}", p.display()))
}

fn default_user_key_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".agent-mesh").join("user.key")
}

fn issue_agent(user: &UserKey, role: &str, capabilities: Vec<String>) -> AgentKey {
    AgentKey::issue(
        user,
        AgentMetadata {
            role: role.into(),
            host: current_hostname(),
            capabilities,
            issued_at: now_rfc3339(),
            expires_at: None,
        },
    )
}

/// Browse mDNS for a peer whose fingerprint matches `prefix` (either a
/// full 64-char hex, the 12-char short form, or any hex prefix). Only
/// peers under the same `user_fp` are considered.
async fn resolve_peer_fp(
    prefix: &str,
    user_fp: Fingerprint,
    deadline: Duration,
) -> Result<Fingerprint> {
    let (_handle, mut rx) = Browser::start()?;
    let timer = tokio::time::sleep(deadline);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            _ = &mut timer => {
                anyhow::bail!(
                    "no peer matching fp prefix '{prefix}' announced within {deadline:?}"
                );
            }
            event = rx.recv() => {
                let Some(event) = event else {
                    anyhow::bail!("browser closed before peer with prefix '{prefix}' appeared");
                };
                if let BrowserEvent::Resolved(peer) = event {
                    if !peer.is_same_user(&user_fp) {
                        continue;
                    }
                    let hex = peer.agent_fp.hex();
                    let short = peer.agent_fp.short();
                    if hex == prefix || hex.starts_with(prefix) || short == prefix {
                        return Ok(peer.agent_fp);
                    }
                }
            }
        }
    }
}

fn current_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn parse_tier(s: &str) -> Result<newt_core::router::Tier> {
    use newt_core::router::Tier;
    match s.to_ascii_uppercase().as_str() {
        "FAST" => Ok(Tier::Fast),
        "STANDARD" => Ok(Tier::Standard),
        "COMPLEX" => Ok(Tier::Complex),
        "REVIEW" => Ok(Tier::Review),
        other => anyhow::bail!("unknown tier '{other}' (use FAST/STANDARD/COMPLEX/REVIEW)"),
    }
}

fn parse_duration(s: &str) -> Result<Duration> {
    if let Some(n) = s.strip_suffix("ms") {
        Ok(Duration::from_millis(n.parse()?))
    } else if let Some(n) = s.strip_suffix('s') {
        Ok(Duration::from_secs(n.parse()?))
    } else if let Some(n) = s.strip_suffix('m') {
        Ok(Duration::from_secs(n.parse::<u64>()? * 60))
    } else {
        Ok(Duration::from_secs(s.parse()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_handles_ms() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
    }

    #[test]
    fn parse_duration_handles_seconds() {
        assert_eq!(parse_duration("7s").unwrap(), Duration::from_secs(7));
    }

    #[test]
    fn parse_duration_handles_minutes() {
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
    }

    #[test]
    fn parse_duration_falls_back_to_seconds_without_suffix() {
        assert_eq!(parse_duration("12").unwrap(), Duration::from_secs(12));
    }

    #[test]
    fn parse_duration_rejects_garbage() {
        assert!(parse_duration("nope").is_err());
    }

    #[test]
    fn parse_tier_accepts_canonical_names() {
        use newt_core::router::Tier;
        assert!(matches!(parse_tier("FAST").unwrap(), Tier::Fast));
        assert!(matches!(parse_tier("standard").unwrap(), Tier::Standard));
        assert!(matches!(parse_tier("Complex").unwrap(), Tier::Complex));
        assert!(matches!(parse_tier("REVIEW").unwrap(), Tier::Review));
    }

    #[test]
    fn parse_tier_rejects_unknown() {
        assert!(parse_tier("frobnicate").is_err());
    }

    #[test]
    fn default_user_key_path_includes_agent_mesh_dir() {
        let p = default_user_key_path();
        assert!(
            p.to_string_lossy().contains(".agent-mesh"),
            "got {}",
            p.display()
        );
        assert!(p.ends_with("user.key"));
    }

    #[test]
    fn current_hostname_returns_nonempty() {
        let h = current_hostname();
        assert!(!h.is_empty());
    }

    #[test]
    fn now_rfc3339_renders_zulu() {
        let s = now_rfc3339();
        assert!(s.ends_with('Z'), "got {s}");
        // YYYY-MM-DDTHH:MM:SSZ is 20 chars.
        assert_eq!(s.len(), 20, "got {s}");
    }
}
