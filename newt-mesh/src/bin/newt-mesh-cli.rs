//! `newt-mesh` CLI binary.
//!
//! Operations:
//!
//! - `newt-mesh announce` — bind a responder service on the LAN that
//!   answers `InferenceRequest`s using the local Ollama backend.
//! - `newt-mesh ask <peer_fp> <prompt>` — resolve a peer by
//!   fingerprint (full or short prefix) via mDNS, then send it an
//!   `InferenceRequest` and print the reply.
//! - `newt-mesh dock-key` — print this installation's dock keys, to copy to
//!   the other end of a dock.
//! - `newt-mesh dock <hub-key>@<addr>` — dock this host to an approved hub
//!   over an uplink it dials, until undocked (K8,
//!   `docs/decisions/newt_web_docking.md`).
//!
//! The trust root is loaded from `~/.agent-mesh/user.key` by default;
//! both subcommands accept a `--user-key` override.
//!
//! This binary lives in the out-of-workspace `newt-mesh` crate so the
//! default newt workspace stays buildable without a side-by-side
//! `../agent-mesh/` checkout. See `docs/decisions/mesh_integration.md`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_mesh_core::{AgentKey, AgentMetadata, Caveats, Fingerprint, UserKey};
use agent_mesh_discovery::{Browser, BrowserEvent};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use newt_inference::backend::InferenceBackend;
use newt_inference::local::LocalOllamaBackend;
use newt_mesh::{InferenceRequest, MeshAsker, NewtMeshService, CAPABILITY_TAG};

#[path = "newt-mesh/help_suite.rs"]
mod help_suite;

/// Default model when the user doesn't override it via env or flag.
const DEFAULT_MODEL: &str = "llama3.1:8b";

#[derive(Parser, Debug)]
#[command(
    name = "newt-mesh",
    version,
    about = "Mesh inference dispatch for newt-agent (announce + ask)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Bind a responder service: announce this newt on the LAN and
    /// answer inference requests from peers.
    Announce {
        /// Extra capability tags to advertise (`newt-inference` and
        /// `model=<id>` are always included).
        #[arg(long = "capability")]
        capabilities: Vec<String>,
        /// Bind port (`0` lets the OS choose).
        #[arg(long, default_value = "0")]
        port: u16,
        /// Path to the user key (defaults to `~/.agent-mesh/user.key`).
        #[arg(long)]
        user_key: Option<PathBuf>,
        /// Role label.
        #[arg(long, default_value = "newt-worker")]
        role: String,
        /// Model to serve (defaults to `llama3.1:8b`).
        #[arg(long)]
        model: Option<String>,
    },
    /// Send an inference request to a peer newt and print the reply.
    Ask {
        /// Peer agent fingerprint — full 64-char hex, 12-char short
        /// form, or any hex prefix.
        peer_fp: String,
        /// The prompt to ask.
        prompt: String,
        /// Tier hint (FAST/STANDARD/COMPLEX/REVIEW).
        #[arg(long)]
        tier: Option<String>,
        /// Pin the model — responder must serve this exact model or
        /// return an error.
        #[arg(long)]
        model: Option<String>,
        /// Max output tokens.
        #[arg(long)]
        max_tokens: Option<u32>,
        /// Path to the user key (defaults to `~/.agent-mesh/user.key`).
        #[arg(long)]
        user_key: Option<PathBuf>,
        /// How long to wait for the peer + reply. Accepts `Ns`, `Nm`,
        /// `Nms`, or a bare integer (seconds).
        #[arg(long, default_value = "30s")]
        timeout: String,
    },
    /// Print this installation's dock keys — as a hub and as a host — and
    /// their words, to copy to the other end of a dock.
    DockKey {
        /// The newt state dir (default: the newt config dir, `~/.newt`).
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
    /// Dock this host to a hub: dial it and serve its dock requests over that
    /// uplink until the hub is revoked here (`newt dock revoke`), dock exposure
    /// is disabled (`/dock disable`), or you press Ctrl-C. The first time, the
    /// host and hub pair by Numeric Comparison: both show a 6-digit code, and
    /// each side is approved only when its operator confirms the codes match.
    Dock {
        /// The hub, as `<hub-key>@<ip>:<port>`: the hub key its
        /// `newt-mesh dock-key` prints, and its `NEWT_WEB_DOCK_UPLINK_PORT`.
        hub: String,
        /// Let the hub inject prompts too (`mirror-inject`). Default: mirror.
        #[arg(long)]
        inject: bool,
        /// A label for the hub in this host's dock registry.
        #[arg(long, default_value = "hub")]
        label: String,
        /// The newt state dir (default: the newt config dir, `~/.newt`).
        #[arg(long)]
        state_dir: Option<PathBuf>,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let cli = help_suite::parse_with_help::<Cli>()?;
    match cli.command {
        Command::Announce {
            capabilities,
            port,
            user_key,
            role,
            model,
        } => announce(user_key, capabilities, port, role, model).await,
        Command::Ask {
            peer_fp,
            prompt,
            tier,
            model,
            max_tokens,
            user_key,
            timeout,
        } => ask(user_key, peer_fp, prompt, tier, model, max_tokens, timeout).await,
        Command::DockKey { state_dir } => dock_key(&state_dir_or_default(state_dir)?),
        Command::Dock {
            hub,
            inject,
            label,
            state_dir,
        } => dock(&hub, inject, &label, state_dir_or_default(state_dir)?).await,
    }
}

/// The newt state dir: `state_dir`, or the newt config dir where `newt dock`
/// keeps its registry and the operator identity lives.
fn state_dir_or_default(state_dir: Option<PathBuf>) -> Result<PathBuf> {
    state_dir
        .or_else(newt_core::Config::user_config_dir)
        .context("cannot locate the newt state dir; pass --state-dir")
}

/// The operator identity and dock instance a dock key is derived from.
fn dock_identity(state_dir: &std::path::Path) -> Result<(UserKey, String)> {
    let identity = state_dir.join("identity.pem");
    let user = UserKey::load(&identity)
        .with_context(|| format!("load the operator identity {}", identity.display()))?;
    let instance = newt_mesh::dock_instance(state_dir).context("read the dock instance name")?;
    Ok((user, instance))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Run the `dock-key` subcommand.
fn dock_key(state_dir: &std::path::Path) -> Result<()> {
    let (user, instance) = dock_identity(state_dir)?;
    println!("dock instance: {instance}");
    for role in [newt_mesh::DockRole::Hub, newt_mesh::DockRole::Host] {
        let key = newt_mesh::dock_agent(&user, role, &instance).public_bytes();
        println!(
            "{:<4} key: {}\n          words: {}",
            role.name(),
            hex(&key),
            newt_core::dock_registry::pubkey_words(&key).join(" ")
        );
    }
    Ok(())
}

/// Parse `<hub-key>@<ip>:<port>` into the hub's dial endpoint.
fn parse_hub(spec: &str) -> Result<newt_mesh::PeerEndpoint> {
    let (key, addr) = spec
        .split_once('@')
        .context("the hub is `<hub-key>@<ip>:<port>`")?;
    let key = newt_core::dock_registry::decode_agent_pubkey(key)
        .context("the hub key must be 64 hex characters")?;
    let addr: std::net::SocketAddr = addr
        .parse()
        .with_context(|| format!("`{addr}` is not an `<ip>:<port>`"))?;
    Ok(newt_mesh::PeerEndpoint::new(key, addr))
}

/// Run the `dock` subcommand.
async fn dock(hub_spec: &str, inject: bool, label: &str, state_dir: PathBuf) -> Result<()> {
    use newt_core::dock_registry::{self as registry, DockScope};
    use newt_mesh::{HubStanding, UplinkState};
    let hub = parse_hub(hub_spec)?;
    let hub_key = hub.agent_pubkey;
    let hub_fp = registry::agent_fingerprint_of_pubkey(&hub_key);
    if newt_mesh::hub_standing(&state_dir, &hub_fp) == HubStanding::Disabled {
        anyhow::bail!("dock exposure is disabled on this host (`/dock enable` to allow it)");
    }
    let (user, instance) = dock_identity(&state_dir)?;
    println!(
        "docking host `{instance}` to hub {}\n  hub words: {}",
        hub.addr,
        registry::pubkey_words(&hub_key).join(" ")
    );
    let mut uplink = newt_mesh::DockUplink::start(&user, &instance, state_dir.clone(), hub).await?;
    let mut shown = None;
    let mut state = uplink.state();
    loop {
        println!("uplink: {state:?}");
        if let Some(pairing) = uplink.pairing().filter(|p| shown.as_ref() != Some(p)) {
            println!(
                "pairing code: {}\nThe hub shows this SAME code on `newt dock approve --staged`.",
                pairing.code
            );
            if newt_mesh::hub_standing(&state_dir, &hub_fp) == HubStanding::NotApproved {
                let scope = if inject {
                    DockScope::MirrorInject
                } else {
                    DockScope::Mirror
                };
                let prompt = format!(
                    "Does the hub show pairing code {}? Approve it here ({})?",
                    pairing.code,
                    scope.as_wire()
                );
                let confirmed = tokio::task::spawn_blocking(move || confirm_at_terminal(&prompt))
                    .await
                    .unwrap_or(false);
                if !confirmed {
                    println!("not approved; closing the uplink");
                    uplink.close().await;
                    return Ok(());
                }
                registry::approve_dock_with_identity(
                    &state_dir.join("config.toml"),
                    &state_dir.join("identity.pem"),
                    &hub_fp,
                    label,
                    &hex(&hub_key),
                    scope,
                    &pairing.transcript_id,
                )?;
                println!("approved the hub here; it is served once it promotes this host");
            }
            shown = Some(pairing);
        }
        if matches!(state, UplinkState::Undocked | UplinkState::Closed) {
            break;
        }
        state = tokio::select! {
            next = uplink.changed() => next,
            _ = tokio::signal::ctrl_c() => {
                println!("closing the uplink…");
                uplink.close().await;
                return Ok(());
            }
        };
    }
    uplink.close().await;
    if state == UplinkState::Undocked {
        println!("undocked: this host no longer serves the hub");
    }
    Ok(())
}

/// Ask `question` at this terminal; blank or no terminal declines.
fn confirm_at_terminal(question: &str) -> bool {
    let window = newt_core::tty::Terminal::suspend_for_prompt(
        newt_core::tty::TerminalTaker::PlainCliConfirm,
    );
    newt_core::interaction_terminal::confirmed_on_terminal(
        &window,
        &newt_core::interaction_form::confirm(question.to_owned(), "", "yes, they match", "no"),
        false,
    )
}

/// Run the `announce` subcommand.
async fn announce(
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
#[allow(clippy::too_many_arguments)]
async fn ask(
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
            caveats: Caveats::top(),
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
    fn parse_hub_takes_a_key_and_an_address() {
        let key = "ab".repeat(32);
        let hub = parse_hub(&format!("{key}@10.0.0.5:7000")).unwrap();
        assert_eq!(hub.agent_pubkey, [0xab; 32]);
        assert_eq!(hub.addr.to_string(), "10.0.0.5:7000");
        assert!(parse_hub(&format!("{key}10.0.0.5:7000")).is_err(), "no @");
        assert!(parse_hub("abcd@10.0.0.5:7000").is_err(), "short key");
        assert!(parse_hub(&format!("{key}@home.lab")).is_err(), "no port");
    }

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
        assert_eq!(s.len(), 20, "got {s}");
    }
}
