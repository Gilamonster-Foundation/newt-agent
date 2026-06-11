//! Newt CLI dispatch surface.
//!
//! Subcommands: `code`, `pilot`, `worker`, `mcp`, `doctor`, `config`, `dgx`,
//! `tunings`.
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
mod tuning_cmd;

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

    /// Enable deep inference diagnostics for backend compatibility failures.
    /// Implies `--debug` and emits structural response details intended for
    /// GitHub issues and backend debugging. Also set via `NEWT_TRACE=1`.
    #[arg(long, global = true, default_value_t = false)]
    pub trace: bool,

    /// Start the TUI coder with a named persona from ~/.newt/personas/<name>.md.
    /// Defaults are created lazily the first time persona mode is used.
    #[arg(long, global = true, value_name = "NAME")]
    pub persona: Option<String>,

    /// Run with NO conversation persistence: nothing is auto-resumed, no
    /// conversation row is created, and no turn is saved. Equivalent to
    /// setting `NEWT_EPHEMERAL=1`. Takes precedence over
    /// `NEWT_CONVERSATION_ID` and `[conversations] resume` (Step 17.7).
    #[arg(long, global = true, default_value_t = false)]
    pub ephemeral: bool,

    /// Cap the Ollama context window (KV-cache) to this many tokens.
    /// Prevents VRAM exhaustion on large models by limiting how much memory
    /// Ollama allocates for the attention cache. Equivalent to setting
    /// `NEWT_NUM_CTX=<N>` or `[tui] num_ctx = <N>` in newt.toml.
    /// Recommended starting point: 8192. Omit to use the model default.
    #[arg(long, global = true, value_name = "TOKENS")]
    pub num_ctx: Option<u32>,

    /// Directory to search for AGENTS.md/CLAUDE.md, or a specific instructions
    /// file. Default: the workspace (`./`). Also `[agents] path`.
    #[arg(long, global = true, value_name = "PATH")]
    pub agents_file: Option<String>,

    /// Don't load AGENTS.md/CLAUDE.md into the system prompt (overrides
    /// `[agents] enabled`).
    #[arg(long, global = true, default_value_t = false)]
    pub no_agents_file: bool,

    /// Activate a Python virtual environment for all agent-run commands.
    /// Injects `VIRTUAL_ENV` and prepends the venv's `bin/` to `PATH` inside
    /// the confined shell, and grants exec permission for every executable in
    /// the venv's `bin/` — a fast alternative to listing them one-by-one in
    /// `[tui.permissions] extra_exec`. If omitted and `$VIRTUAL_ENV` is already
    /// set (shell-activated venv), that environment is used automatically.
    #[arg(long, global = true, value_name = "PATH")]
    pub venv: Option<PathBuf>,

    /// Grant the agent exec permission for all executables in `<DIR>` and
    /// prepend `<DIR>` to `PATH` in the confined shell. May be repeated.
    /// Example: `newt --exec-path ~/bin --exec-path ~/workspaces/bin`.
    #[arg(long = "exec-path", global = true, value_name = "DIR")]
    pub exec_paths: Vec<PathBuf>,

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
    /// Inspect, export, import, and reset per-model context-window tuning data.
    ///
    /// Tuning data is maintained automatically by the harness in
    /// `~/.newt/model-capabilities.json`. Human-readable overrides live in
    /// `~/.newt/config.toml` under `[[model_tuning]]`. Community profiles can be
    /// shared as plain TOML files and merged with `newt tunings import`.
    Tunings {
        #[command(subcommand)]
        cmd: tuning_cmd::TuningsCmd,
    },
}

pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    // Resolve the venv: --venv flag wins, then fall back to an already-activated $VIRTUAL_ENV.
    // Set NEWT_VENV so the TUI can inject it into the agent-bridle confined shell (which does
    // not inherit the host environment, so a process-level PATH change has no effect there).
    let venv_path = cli
        .venv
        .as_ref()
        .map(|p| p.display().to_string())
        .or_else(|| std::env::var("VIRTUAL_ENV").ok());
    // SAFETY: all set_var calls below are single-threaded before the TUI starts any async work.
    if let Some(ref venv) = venv_path {
        unsafe { std::env::set_var("NEWT_VENV", venv) };
        // Also prepend to the process PATH for non-bridle code paths.
        let venv_bin = format!("{venv}/bin");
        let mut path = std::env::var("PATH").unwrap_or_default();
        if !path.split(':').any(|p| p == venv_bin) {
            path = format!("{venv_bin}:{path}");
        }
        unsafe { std::env::set_var("PATH", path) };
    }

    // --exec-path: store as colon-separated NEWT_EXEC_PATHS so the TUI can
    // scan those directories and grant exec permission for their binaries.
    if !cli.exec_paths.is_empty() {
        let joined = cli
            .exec_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        unsafe { std::env::set_var("NEWT_EXEC_PATHS", &joined) };
    }

    match cli.command.unwrap_or(Command::Code { path: None }) {
        Command::Code { path } => {
            // --debug / --trace / --num-ctx set env vars so the TUI picks them up
            // without requiring a run_code signature change.
            if cli.debug || cli.trace {
                // SAFETY: single-threaded before the TUI starts any async work.
                unsafe { std::env::set_var("NEWT_DEBUG", "1") };
            }
            if cli.trace {
                unsafe { std::env::set_var("NEWT_TRACE", "1") };
            }
            if let Some(n) = cli.num_ctx {
                unsafe { std::env::set_var("NEWT_NUM_CTX", n.to_string()) };
            }
            // --ephemeral threads to the TUI the same way (Step 17.7): the
            // session start resolution reads NEWT_EPHEMERAL once.
            if cli.ephemeral {
                unsafe { std::env::set_var("NEWT_EPHEMERAL", "1") };
            }
            // --no-agents-file / --agents-file thread to the TUI via env vars.
            if cli.no_agents_file {
                unsafe { std::env::set_var("NEWT_NO_AGENTS_FILE", "1") };
            }
            if let Some(p) = &cli.agents_file {
                unsafe { std::env::set_var("NEWT_AGENTS_FILE", p) };
            }
            // Resolve splash preference: CLI flags override config, and
            // --splash overrides --no-splash (enforced by overrides_with).
            let config_no_splash = newt_core::Config::resolve()
                .ok()
                .and_then(|c| c.tui)
                .map(|t| t.no_splash)
                .unwrap_or(false);
            let no_splash = (cli.no_splash || config_no_splash) && !cli.splash;
            newt_tui::run_code(path.as_deref(), no_splash, cli.persona.as_deref())
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
        Command::Tunings { cmd } => tuning_cmd::run(cmd, cli.config.as_deref()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_global_persona_for_default_code_command() {
        let cli = Cli::try_parse_from(["newt", "--persona", "coder"]).unwrap();

        assert_eq!(cli.persona.as_deref(), Some("coder"));
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_global_persona_for_explicit_code_command() {
        let cli = Cli::try_parse_from(["newt", "code", "--persona", "reviewer"]).unwrap();

        assert_eq!(cli.persona.as_deref(), Some("reviewer"));
        assert!(matches!(cli.command, Some(Command::Code { .. })));
    }

    #[test]
    fn parses_debug_and_num_ctx_globals() {
        let cli = Cli::try_parse_from(["newt", "--debug", "--num-ctx", "8192"]).unwrap();

        assert!(cli.debug);
        assert_eq!(cli.num_ctx, Some(8192));
    }

    #[test]
    fn parses_trace_global() {
        let cli = Cli::try_parse_from(["newt", "--trace"]).unwrap();

        assert!(cli.trace);
    }

    #[test]
    fn parses_ephemeral_global() {
        // 17.7: works bare (default `code` command) and explicit; off by default.
        let cli = Cli::try_parse_from(["newt", "--ephemeral"]).unwrap();
        assert!(cli.ephemeral);
        let cli = Cli::try_parse_from(["newt", "code", "--ephemeral"]).unwrap();
        assert!(cli.ephemeral);
        let cli = Cli::try_parse_from(["newt"]).unwrap();
        assert!(!cli.ephemeral);
    }

    #[test]
    fn parses_agents_file_global() {
        let cli = Cli::try_parse_from(["newt", "--agents-file", "docs/AGENTS.md"]).unwrap();
        assert_eq!(cli.agents_file.as_deref(), Some("docs/AGENTS.md"));
        assert!(!cli.no_agents_file);
    }

    #[test]
    fn parses_no_agents_file_global() {
        let cli = Cli::try_parse_from(["newt", "code", "--no-agents-file"]).unwrap();
        assert!(cli.no_agents_file);
        assert_eq!(cli.agents_file, None);
        assert!(matches!(cli.command, Some(Command::Code { .. })));
    }

    #[test]
    fn parses_venv_and_repeated_exec_paths() {
        let cli = Cli::try_parse_from([
            "newt",
            "--venv",
            "/opt/venv",
            "--exec-path",
            "/home/u/bin",
            "--exec-path",
            "/usr/local/bin",
        ])
        .unwrap();

        assert_eq!(cli.venv.as_deref(), Some(std::path::Path::new("/opt/venv")));
        assert_eq!(
            cli.exec_paths,
            vec![
                PathBuf::from("/home/u/bin"),
                PathBuf::from("/usr/local/bin")
            ]
        );
    }

    #[test]
    fn splash_flag_overrides_no_splash() {
        let cli = Cli::try_parse_from(["newt", "--no-splash", "--splash"]).unwrap();
        assert!(cli.splash);
        assert!(!cli.no_splash);

        // …and the override works in both orders.
        let cli = Cli::try_parse_from(["newt", "--splash", "--no-splash"]).unwrap();
        assert!(cli.no_splash);
        assert!(!cli.splash);
    }

    #[test]
    fn parses_worker_identity_flags() {
        let cli = Cli::try_parse_from([
            "newt",
            "worker",
            "--coder",
            "--operator-key-path",
            "/tmp/id.pem",
            "--allow-no-key",
        ])
        .unwrap();

        match cli.command {
            Some(Command::Worker {
                coder,
                operator_key_path,
                allow_no_key,
            }) => {
                assert!(coder);
                assert_eq!(operator_key_path, Some(PathBuf::from("/tmp/id.pem")));
                assert!(allow_no_key);
            }
            other => panic!("expected worker command, got {other:?}"),
        }
    }

    #[test]
    fn worker_flags_default_to_safe_values() {
        let cli = Cli::try_parse_from(["newt", "worker"]).unwrap();

        match cli.command {
            Some(Command::Worker {
                coder,
                operator_key_path,
                allow_no_key,
            }) => {
                assert!(!coder, "coder plugin must be opt-in");
                assert_eq!(operator_key_path, None);
                assert!(!allow_no_key, "allow-no-key must never be the default");
            }
            other => panic!("expected worker command, got {other:?}"),
        }
    }
}
