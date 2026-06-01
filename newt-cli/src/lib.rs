//! Newt CLI dispatch surface.
//!
//! Subcommands: `code`, `pilot`, `worker`, `mcp`, `doctor`, `config`, `dgx`.
//!
//! The mesh subcommands (`announce`, `ask`) live in a sibling binary,
//! `newt-mesh-cli`, inside the out-of-workspace `newt-mesh/` crate.
//! See `docs/decisions/mesh_integration.md` for why that crate is
//! kept out of the default workspace.

mod config_cmd;
mod dgx;
mod doctor;
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

    /// Skip the full-screen splash: print a compact inline header instead.
    /// Applies to the `code` subcommand (the default). Also configurable
    /// via `[tui] no_splash = true` in newt.toml.
    #[arg(long, global = true, default_value_t = false)]
    pub no_splash: bool,

    /// Subcommand to run. Defaults to `code` (TUI coder) when omitted.
    #[command(subcommand)]
    pub command: Option<Command>,
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
    Worker {
        /// Activate the newt-coder plugin (whole-file emit +
        /// server-side diff normalization). Equivalent to setting
        /// `NEWT_CODER=1` in the environment. Closes failure mode
        /// T0b — see the knowledge card
        /// `~/workspaces/knowledge/board/drake/2026-05-29_newt-coder-failure-mode-taxonomy.md`.
        #[arg(long, env = "NEWT_CODER", default_value_t = false)]
        coder: bool,
    },
    /// MCP server (stdio JSON-RPC, no TUI).
    Mcp,
    /// Health-check local backends + provider plugins.
    Doctor,
    /// Print resolved config.
    Config,
    /// Open the interactive settings TUI.
    Settings,
    /// NVIDIA DGX endpoint management (route a task to a formation; more
    /// subcommands land in later Phase 14 steps).
    Dgx {
        #[command(subcommand)]
        cmd: dgx::DgxCmd,
    },
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command.unwrap_or(Command::Code { path: None }) {
        Command::Code { path } => newt_tui::run_code(path.as_deref(), cli.no_splash),
        Command::Pilot { flight_id } => newt_tui::run_pilot(&flight_id),
        Command::Worker { coder } => run_worker(coder).await,
        Command::Mcp => run_mcp().await,
        Command::Doctor => doctor::run(cli.config.as_deref()).await,
        Command::Config => config_cmd::run(cli.config.as_deref()),
        Command::Settings => newt_tui::run_settings(cli.config.as_deref()),
        Command::Dgx { cmd } => dgx::run(cmd, cli.config.as_deref()).await,
    }
}

/// Spawn the ACP worker with stdio safety.
///
/// On Unix we redirect fd 1 to fd 2 (stderr) and hand the saved real
/// stdout to the server. Any rogue `println!()` from a dependency
/// will land on stderr instead of corrupting the JSON-RPC wire. On
/// non-Unix targets we fall back to plain stdout — a deliberate
/// out-of-scope corner documented in the PR.
///
/// `coder` activates the newt-coder plugin (whole-file emit +
/// server-side diff normalization). The flag is plumbed through to
/// the server via `NEWT_CODER=1`, which `handle_new_session` reads;
/// this is the same env the ACP server already honors, so a user
/// invoking the daemon under systemd can either pass `--coder` or
/// set `NEWT_CODER=1` in the unit file — both work.
async fn run_worker(coder: bool) -> anyhow::Result<()> {
    if coder {
        // SAFETY: single-threaded section before tokio takes over —
        // set_var is safe here because no other thread reads/writes
        // env yet. handle_new_session reads this for every session.
        unsafe {
            std::env::set_var("NEWT_CODER", "1");
        }
        tracing::info!("newt-coder plugin activated (whole-file emit)");
    }

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
