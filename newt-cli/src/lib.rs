//! Newt CLI dispatch surface.
//!
//! Subcommands: `code`, `pilot`, `worker`, `mcp`, `doctor`, `config`.

mod config_cmd;
mod doctor;

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
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Code { path } => newt_tui::run_code(path.as_deref()),
        Command::Pilot { flight_id } => newt_tui::run_pilot(&flight_id),
        Command::Worker => newt_acp_worker::run_stdio().await,
        Command::Mcp => newt_mcp_server::run_stdio().await,
        Command::Doctor => doctor::run(cli.config.as_deref()).await,
        Command::Config => config_cmd::run(cli.config.as_deref()),
    }
}
