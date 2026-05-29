//! Newt CLI dispatch surface.
//!
//! Subcommands: `code`, `pilot`, `worker`, `mcp`, `doctor`, `config`.
//! With `--features mesh`: also `mesh announce` / `mesh ask`.

mod config_cmd;
mod doctor;
#[cfg(feature = "mesh")]
mod mesh;
pub mod stdio_guard;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "newt",
    version,
    about = "Small, fast, local-first agentic coder"
)]
pub struct Cli {
    /// Path to config file (overrides default search order).
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Standalone TUI coder.
    Code {
        /// Optional working path.
        path: Option<PathBuf>,
    },
    /// Drake-swarm pilot dashboard.
    Pilot {
        /// Flight id to attach to.
        flight_id: String,
    },
    /// ACP worker (stdio JSON-RPC, no TUI).
    Worker,
    /// MCP server (stdio JSON-RPC, no TUI).
    Mcp,
    /// Health-check local backends + provider plugins.
    Doctor,
    /// Print resolved config.
    Config,
    /// Mesh operations (requires the `mesh` cargo feature).
    #[cfg(feature = "mesh")]
    Mesh {
        #[command(subcommand)]
        action: MeshAction,
    },
}

/// Subcommands under `newt mesh`. Only compiled with `--features mesh`.
#[cfg(feature = "mesh")]
#[derive(Subcommand, Debug)]
pub enum MeshAction {
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
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Code { path } => newt_tui::run_code(path.as_deref()),
        Command::Pilot { flight_id } => newt_tui::run_pilot(&flight_id),
        Command::Worker => run_worker().await,
        Command::Mcp => run_mcp().await,
        Command::Doctor => doctor::run(cli.config.as_deref()).await,
        Command::Config => config_cmd::run(cli.config.as_deref()),
        #[cfg(feature = "mesh")]
        Command::Mesh { action } => match action {
            MeshAction::Announce {
                capabilities,
                port,
                user_key,
                role,
                model,
            } => mesh::announce(user_key, capabilities, port, role, model).await,
            MeshAction::Ask {
                peer_fp,
                prompt,
                tier,
                model,
                max_tokens,
                user_key,
                timeout,
            } => mesh::ask(user_key, peer_fp, prompt, tier, model, max_tokens, timeout).await,
        },
    }
}

/// Spawn the ACP worker with stdio safety.
///
/// On Unix we redirect fd 1 to fd 2 (stderr) and hand the saved real
/// stdout to the server. Any rogue `println!()` from a dependency
/// will land on stderr instead of corrupting the JSON-RPC wire. On
/// non-Unix targets we fall back to plain stdout — a deliberate
/// out-of-scope corner documented in the PR.
async fn run_worker() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        match stdio_guard::redirect_stdout_to_stderr() {
            Ok(private_stdout) => {
                let tokio_stdout = tokio::fs::File::from_std(private_stdout);
                newt_acp_worker::run_with_io(tokio::io::stdin(), tokio_stdout).await
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "stdio_guard fd redirect failed; falling back to raw stdout"
                );
                newt_acp_worker::run_stdio().await
            }
        }
    }
    #[cfg(not(unix))]
    {
        newt_acp_worker::run_stdio().await
    }
}

/// Spawn the MCP server with the same stdio safety dance as
/// [`run_worker`].
async fn run_mcp() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        match stdio_guard::redirect_stdout_to_stderr() {
            Ok(private_stdout) => {
                let tokio_stdout = tokio::fs::File::from_std(private_stdout);
                newt_mcp_server::run_with_io(tokio::io::stdin(), tokio_stdout).await
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "stdio_guard fd redirect failed; falling back to raw stdout"
                );
                newt_mcp_server::run_stdio().await
            }
        }
    }
    #[cfg(not(unix))]
    {
        newt_mcp_server::run_stdio().await
    }
}
