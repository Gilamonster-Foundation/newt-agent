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
mod skills;
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
    /// via `[tui] no_splash = true` in newt.toml. Overrides `--splash`.
    #[arg(
        long,
        global = true,
        default_value_t = false,
        overrides_with = "splash"
    )]
    pub no_splash: bool,

    /// Force the full-screen splash even when `[tui] no_splash = true` is set
    /// in the config. Overrides `--no-splash`.
    #[arg(
        long,
        global = true,
        default_value_t = false,
        overrides_with = "no_splash"
    )]
    pub splash: bool,

    /// Enable per-round agent-loop diagnostics: prints each round's content
    /// excerpt, tool-call count, and token usage. Also enables fallback
    /// messages when the model returns an empty reply. Equivalent to setting
    /// `NEWT_DEBUG=1` in the environment or `[tui] debug = true` in newt.toml.
    #[arg(long, global = true, default_value_t = false)]
    pub debug: bool,

    /// Cap the Ollama context window (KV-cache) to this many tokens.
    /// Prevents VRAM exhaustion on large models by limiting how much memory
    /// Ollama allocates for the attention cache. Equivalent to setting
    /// `NEWT_NUM_CTX=<N>` or `[tui] num_ctx = <N>` in newt.toml.
    /// Recommended starting point: 8192. Omit to use the model default.
    #[arg(long, global = true, value_name = "TOKENS")]
    pub num_ctx: Option<u32>,

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

        /// Per-user operator key path the headless worker derives its
        /// signed, attenuated dispatch [`newt_core::Caveats`] from.
        /// Default: `~/.newt/identity.pem` (resolved via
        /// `newt_identity::default_key_path`). Generated on first run
        /// with mode `0600` if the file doesn't yet exist.
        ///
        /// CLI > env > default file resolution. The env override is
        /// `NEWT_OPERATOR_KEY`. Issue #94.
        #[arg(long, env = "NEWT_OPERATOR_KEY")]
        operator_key_path: Option<PathBuf>,

        /// Debug-only escape hatch: skip the operator-key load and
        /// dispatch under `Caveats::top()` (pre-#94 behavior). Never
        /// the default. Use this when iterating locally without
        /// provisioning a key — never in production.
        #[arg(long, default_value_t = false)]
        allow_no_key: bool,
    },
    /// MCP server (stdio JSON-RPC, no TUI).
    Mcp,
    /// Health-check local backends + provider plugins.
    Doctor,
    /// Print resolved config.
    Config,
    /// Run (or re-run) the setup wizard: probe Ollama + write ~/.newt/config.toml.
    /// Edit that file directly for everything else — newt has no settings UI.
    Init,
    /// Interactive first-run setup: choose Ollama or DGX, pick a model from the
    /// endpoint, preview, and write ~/.newt/config.toml. Unlike `init` (which
    /// silently auto-probes Ollama), this prompts the human through each choice.
    Setup,
    /// Manage skills across a configurable search path (newt + Claude Code +
    /// Codex + …). `list` / `install <path>` / `share`.
    Skills {
        #[command(subcommand)]
        cmd: skills::SkillsCmd,
    },
    /// NVIDIA DGX endpoint management (route a task to a formation; more
    /// subcommands land in later Phase 14 steps).
    Dgx {
        #[command(subcommand)]
        cmd: dgx::DgxCmd,
    },
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command.unwrap_or(Command::Code { path: None }) {
        Command::Code { path } => {
            // --debug / --num-ctx set env vars so the TUI picks them up
            // without requiring a run_code signature change.
            if cli.debug {
                // SAFETY: single-threaded before the TUI starts any async work.
                unsafe { std::env::set_var("NEWT_DEBUG", "1") };
            }
            if let Some(n) = cli.num_ctx {
                unsafe { std::env::set_var("NEWT_NUM_CTX", n.to_string()) };
            }
            // Resolve splash preference: CLI flags override config, and
            // --splash overrides --no-splash (enforced by overrides_with).
            let config_no_splash = newt_core::Config::resolve()
                .ok()
                .and_then(|c| c.tui)
                .map(|t| t.no_splash)
                .unwrap_or(false);
            let no_splash = (cli.no_splash || config_no_splash) && !cli.splash;
            newt_tui::run_code(path.as_deref(), no_splash)
        }
        Command::Pilot { flight_id } => newt_tui::run_pilot(&flight_id),
        Command::Worker {
            coder,
            operator_key_path,
            allow_no_key,
        } => run_worker(coder, operator_key_path, allow_no_key).await,
        Command::Mcp => run_mcp().await,
        Command::Doctor => doctor::run(cli.config.as_deref()).await,
        Command::Config => config_cmd::run(cli.config.as_deref()),
        Command::Init => newt_tui::run_init(newt_tui::color_supported()),
        Command::Setup => newt_tui::run_setup(newt_tui::color_supported()),
        Command::Skills { cmd } => skills::run(cmd, cli.config.as_deref()),
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
///
/// `operator_key_path` and `allow_no_key` plumb the worker's signed
/// operator identity (#94). The headless worker derives an attenuated,
/// signed [`newt_core::Caveats`] from that identity per dispatch instead
/// of dispatching under `Caveats::top()`. CLI > env > default-file
/// resolution. On any unresolved-key failure without `--allow-no-key`,
/// the worker refuses to start.
async fn run_worker(
    coder: bool,
    operator_key_path: Option<PathBuf>,
    allow_no_key: bool,
) -> anyhow::Result<()> {
    if coder {
        // SAFETY: single-threaded section before tokio takes over —
        // set_var is safe here because no other thread reads/writes
        // env yet. handle_new_session reads this for every session.
        unsafe {
            std::env::set_var("NEWT_CODER", "1");
        }
        tracing::info!("newt-coder plugin activated (whole-file emit)");
    }

    // Resolve the operator identity once, BEFORE any tokio work, so a
    // missing-key refusal fails fast and never tries to drain stdin.
    let identity =
        newt_acp_worker::WorkerIdentity::resolve(operator_key_path.as_deref(), allow_no_key)
            .map_err(|e| {
                anyhow::anyhow!(
                    "headless worker refused to start: {e}\n\
                     hint: pass --operator-key-path <PEM>, set NEWT_OPERATOR_KEY, \
                     or use --allow-no-key (debug only) to fall back to top()"
                )
            })?;

    if !identity.is_operator() {
        tracing::warn!(
            "headless worker started with --allow-no-key: dispatching under \
             unbounded debug authority (debug-only fallback, never the default)"
        );
    } else {
        tracing::info!("headless worker started with operator-rooted identity");
    }

    // Start the Prometheus /metrics endpoint if NEWT_METRICS_PORT is set.
    // The registry lives for the lifetime of the worker process.
    let metrics = maybe_start_metrics_server();

    #[cfg(unix)]
    {
        match stdio_guard::redirect_stdout_to_stderr() {
            Ok(private_stdout) => {
                let tokio_stdout = tokio::fs::File::from_std(private_stdout);
                newt_acp_worker::run_with_io_metrics_and_identity(
                    tokio::io::stdin(),
                    tokio_stdout,
                    metrics,
                    identity,
                )
                .await
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "stdio_guard fd redirect failed; falling back to raw stdout"
                );
                newt_acp_worker::run_with_io_metrics_and_identity(
                    tokio::io::stdin(),
                    tokio::io::stdout(),
                    metrics,
                    identity,
                )
                .await
            }
        }
    }
    #[cfg(not(unix))]
    {
        newt_acp_worker::run_with_io_metrics_and_identity(
            tokio::io::stdin(),
            tokio::io::stdout(),
            metrics,
            identity,
        )
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
