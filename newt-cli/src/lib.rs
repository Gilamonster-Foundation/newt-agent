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
    /// Run (or re-run) the first-time setup wizard to configure ~/.newt/.
    Init,
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
        Command::Init => newt_tui::run_init(newt_tui::color_supported()),
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

    // Start the Prometheus /metrics endpoint if NEWT_METRICS_PORT is set.
    // The registry lives for the lifetime of the worker process.
    let metrics = maybe_start_metrics_server();

    #[cfg(unix)]
    {
        match stdio_guard::redirect_stdout_to_stderr() {
            Ok(private_stdout) => {
                let tokio_stdout = tokio::fs::File::from_std(private_stdout);
                newt_acp_worker::run_with_io_and_metrics(tokio::io::stdin(), tokio_stdout, metrics)
                    .await
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "stdio_guard fd redirect failed; falling back to raw stdout"
                );
                newt_acp_worker::run_with_io_and_metrics(
                    tokio::io::stdin(),
                    tokio::io::stdout(),
                    metrics,
                )
                .await
            }
        }
    }
    #[cfg(not(unix))]
    {
        newt_acp_worker::run_with_io_and_metrics(tokio::io::stdin(), tokio::io::stdout(), metrics)
            .await
    }
}

/// Check `NEWT_METRICS_PORT`; if set, create a metrics registry and spawn the
/// HTTP scrape server as a background task.
///
/// Returns the registry for injection into the ACP server, or `None` if the
/// env var is absent or invalid.
fn maybe_start_metrics_server() -> Option<std::sync::Arc<newt_acp_worker::NewtMetrics>> {
    let port: u16 = std::env::var("NEWT_METRICS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&p| p > 0)?;

    let registry = match newt_acp_worker::NewtMetrics::new() {
        Ok(r) => std::sync::Arc::new(r),
        Err(e) => {
            tracing::warn!(error = %e, "failed to create Prometheus registry — metrics disabled");
            return None;
        }
    };

    let reg = registry.clone();
    tokio::spawn(async move {
        newt_acp_worker::prom::serve(port, reg).await;
    });

    tracing::info!(port, "Prometheus metrics server started");
    Some(registry)
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

#[cfg(test)]
mod tests {
    use super::maybe_start_metrics_server;

    /// `maybe_start_metrics_server` is gated entirely on `NEWT_METRICS_PORT`:
    /// absent / unparseable / zero → `None`; a valid port → `Some(registry)`
    /// and the scrape server is spawned. Exercised in one test so the
    /// (process-global) env var isn't raced by parallel cases.
    #[tokio::test]
    async fn maybe_start_metrics_server_honors_env() {
        // SAFETY: single-purpose test; no other test touches NEWT_METRICS_PORT.
        unsafe { std::env::remove_var("NEWT_METRICS_PORT") };
        assert!(maybe_start_metrics_server().is_none(), "absent env → None");

        unsafe { std::env::set_var("NEWT_METRICS_PORT", "not-a-port") };
        assert!(maybe_start_metrics_server().is_none(), "unparseable → None");

        unsafe { std::env::set_var("NEWT_METRICS_PORT", "0") };
        assert!(maybe_start_metrics_server().is_none(), "port 0 → None");

        // A real free port → Some(registry); the spawn happens on the tokio
        // runtime this test provides.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        unsafe { std::env::set_var("NEWT_METRICS_PORT", port.to_string()) };
        assert!(
            maybe_start_metrics_server().is_some(),
            "valid port → Some(registry)"
        );
        unsafe { std::env::remove_var("NEWT_METRICS_PORT") };
    }
}
