//! Newt CLI dispatch surface.
//!
//! Subcommands: `code`, `pilot`, `worker`, `mcp`, `doctor`, `config`,
//! `identity`, `dgx`, `tunings`.
//!
//! The mesh subcommands (`announce`, `ask`) live in a sibling binary,
//! `newt-mesh-cli`, inside the out-of-workspace `newt-mesh/` crate.
//! See `docs/decisions/mesh_integration.md` for why that crate is
//! kept out of the default workspace.

mod auth_cmd;
mod config_cmd;
pub mod crew;
pub mod crew_runner;
mod dgx;
mod doctor;
mod identity_cmd;
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

    /// Disable the input footer (the transient multi-line `❯` block + status
    /// header) and use a plain bash-like prompt. Equivalent to `NEWT_FOOTER=off`
    /// or `[tui] footer = "off"`. By default the footer shows on a TTY and
    /// auto-degrades to a plain scroller off one (pipes, `newt worker`).
    #[arg(
        long,
        visible_alias = "no-footer",
        global = true,
        default_value_t = false
    )]
    pub plain: bool,

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

    /// When a tool call is denied by the session's permission caveats, ask
    /// interactively — allow once / allow for this session / deny — instead
    /// of failing the call outright (issue #263). Decisions are recorded to
    /// `~/.newt/permission-log.jsonl` for later review (`/permissions` lists
    /// them). Equivalent to `[tui.permissions] prompt = true`. Interactive
    /// TUI only: headless runs (worker / eval) always keep the plain denial.
    #[arg(long, global = true, default_value_t = false)]
    pub prompt_for_permissions: bool,

    /// INTERIM (#297): disable the ocap confined shell for THIS invocation —
    /// run_command executes unconfined on the plain host shell (same venv/PATH
    /// handling, same output shape). fs tools keep the workspace fence and
    /// web_fetch keeps its leash: this is unconfined exec, not authority-off.
    /// Equivalent to NEWT_DISABLE_OCAP=1; deliberately NO config-file key, so
    /// the bypass must be asserted per invocation. Removed (or demoted to a
    /// debug flag) once brush upstreams CommandInterceptor and agent-bridle's
    /// real confined shell works everywhere (agent-bridle#20).
    #[arg(long, visible_alias = "yolo", global = true, default_value_t = false)]
    pub disable_ocap: bool,

    /// Cap the Ollama context window (KV-cache) to this many tokens.
    /// Prevents VRAM exhaustion on large models by limiting how much memory
    /// Ollama allocates for the attention cache. Equivalent to setting
    /// `NEWT_NUM_CTX=<N>` or `[tui] num_ctx = <N>` in newt.toml.
    /// Recommended starting point: 8192. Omit to use the model default.
    #[arg(long, global = true, value_name = "TOKENS")]
    pub num_ctx: Option<u32>,

    /// Apply a named profile (`[profiles.<name>]` in newt.toml) — a composition
    /// of harness techniques + their knob settings tuned for a model family /
    /// context. Equivalent to `NEWT_PROFILE=<name>`. An unknown profile, or one
    /// naming an unknown technique, is a hard error. Omit for default behavior.
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Load a named bundle (`[bundles.<name>]`) — the loadable unit of the model
    /// support kit. The bundle resolves to a profile for the active model (its
    /// `families` map, else `default_profile`). `--profile` overrides it; with
    /// neither, a bundle whose `applies_to` matches the model is auto-inferred.
    /// Equivalent to `NEWT_BUNDLE=<name>`. An unknown bundle is a hard error.
    #[arg(long, global = true, value_name = "NAME")]
    pub bundle: Option<String>,

    /// Load a named loadout (`[loadouts.<name>]`) — the full composition of
    /// provider → model → kit → role → settings. Each axis is fed to its existing
    /// selector; an explicit `--profile`/`--bundle`/`--num-ctx`/`--persona`
    /// overrides the corresponding loadout field. Equivalent to `NEWT_LOADOUT=<name>`.
    /// An unknown loadout or a dangling reference inside it is a hard error.
    #[arg(long, global = true, value_name = "NAME")]
    pub loadout: Option<String>,

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

    /// Grant the agent READ access to a file or directory OUTSIDE the workspace.
    /// A directory grants everything under it; a file grants just that file. May
    /// be repeated. Reference the path by its absolute path in tools.
    /// Example: `newt --read ~/.newt --read ~/.hotseat/config.yml`.
    #[arg(long = "read", global = true, value_name = "PATH")]
    pub read_paths: Vec<PathBuf>,

    /// Grant the agent READ+WRITE access to a file or directory OUTSIDE the
    /// workspace (implies `--read` for the same path). May be repeated.
    /// Example: `newt --write ~/scratch`.
    #[arg(long = "write", global = true, value_name = "PATH")]
    pub write_paths: Vec<PathBuf>,

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
    /// Run a multi-LLM crew on a task (navigate → plan → verify → triage), in an
    /// isolated git worktree. Exit 0 = passed, 2 = needs human review, 1 = error.
    Crew {
        /// The task for the crew (a coding-task description). In `--edit` mode
        /// this slot is instead the crew NAME to edit (or use `--crew`).
        task: Option<String>,
        /// Edit a crew's settings interactively (planner/navigator/triage
        /// loadouts, control loop, test command, budgets) and write
        /// `~/.newt/crews/<name>.toml`. No task is run.
        #[arg(long, default_value_t = false)]
        edit: bool,
        /// Crew name from `[crews.<name>]`. Omit when exactly one crew is defined.
        #[arg(long)]
        crew: Option<String>,
        /// Target repo dir (default: current dir). Must be a git repo.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Verification command override (default: the crew's `test`, else inferred).
        #[arg(long)]
        test: Option<String>,
        /// Cap on planning rounds (default: the crew's budget, else 3).
        #[arg(long)]
        max_attempts: Option<u32>,
        /// Resolve + show placements and exit, without editing or testing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Health-check local backends + provider plugins.
    Doctor,
    /// Print resolved config.
    Config,
    /// Print the resolved agent commit identity (`.newt/agent-identity.toml`):
    /// name, email, the layer it resolved from, the signing-key path +
    /// fingerprint (if it loads), the GitHub App's public coordinates, and the
    /// configured token NAMES. Never prints a secret value.
    Identity,
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
    /// Authenticate an HTTP MCP server via the OAuth 2.1 PKCE browser flow.
    ///
    /// Without arguments: lists all discovered HTTP MCP servers and their token
    /// status (valid / expired / needs-login / unregistered).
    ///
    /// With a server name: opens the browser for the OAuth login flow, waits
    /// for the redirect, exchanges the code for tokens, and saves them to
    /// `~/.hermes/mcp-tokens/`. Both newt and hermes-agent share the same token
    /// store, so this authenticates both.
    Auth {
        /// Name of the MCP server to authenticate (e.g. `newt auth my-server`).
        /// Omit to list all servers and their current auth status.
        server: Option<String>,
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

/// Resolve the user's home directory cross-platform: `$HOME` (set on Unix and
/// many Windows shells) first, then `%USERPROFILE%` (the Windows default). Empty
/// values are treated as unset.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()))
        .map(PathBuf::from)
}

/// Absolutise a single `--read`/`--write` grant: expand a leading `~` (via
/// [`home_dir`]), then make it absolute relative to the current dir. Does NOT
/// require the path to exist (a `--write` target may be created later) and does
/// not canonicalise (which would resolve symlinks and need existence) — the goal
/// is a stable absolute path the fs tools' `workspace.join(path)` results can be
/// contained under.
fn abs_grant_path(p: &std::path::Path) -> PathBuf {
    let expanded: PathBuf = match p.strip_prefix("~") {
        Ok(rest) => match home_dir() {
            Some(home) => home.join(rest),
            None => p.to_path_buf(),
        },
        Err(_) => p.to_path_buf(),
    };
    if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()
            .map(|d| d.join(&expanded))
            .unwrap_or(expanded)
    }
}

/// Join absolutised grant paths into a single `NEWT_READ_PATHS` /
/// `NEWT_WRITE_PATHS` value using the platform path-list separator (`;` on
/// Windows, `:` elsewhere) via [`std::env::join_paths`], so a Windows
/// drive-letter path (`C:\…`) is not shattered on a literal `:`. Returns `Err`
/// (fail-closed) if a grant path itself contains the separator.
fn abs_grant_paths(paths: &[PathBuf]) -> Result<std::ffi::OsString, std::env::JoinPathsError> {
    std::env::join_paths(paths.iter().map(|p| abs_grant_path(p)))
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

    // --read / --write: store absolutised paths (joined with the platform
    // path-list separator) so the TUI can widen the agent's fs_read / fs_write
    // scope to these out-of-workspace locations (a dir grants everything under
    // it; a file grants just itself). --write implies --read for the same path.
    if !cli.read_paths.is_empty() {
        match abs_grant_paths(&cli.read_paths) {
            Ok(joined) => unsafe { std::env::set_var("NEWT_READ_PATHS", &joined) },
            Err(e) => {
                eprintln!(
                    "warning: ignoring --read grants (path contains the path separator): {e}"
                );
            }
        }
    }
    if !cli.write_paths.is_empty() {
        match abs_grant_paths(&cli.write_paths) {
            Ok(joined) => unsafe { std::env::set_var("NEWT_WRITE_PATHS", &joined) },
            Err(e) => eprintln!(
                "warning: ignoring --write grants (path contains the path separator): {e}"
            ),
        }
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
            if let Some(p) = cli.profile.as_deref().filter(|p| !p.is_empty()) {
                // --profile threads to the TUI the same way; run_chat resolves +
                // validates it against [profiles.<name>] once per session.
                unsafe { std::env::set_var("NEWT_PROFILE", p) };
            }
            if let Some(b) = cli.bundle.as_deref().filter(|b| !b.is_empty()) {
                // --bundle threads the same way; run_chat resolves it → a profile
                // for the active model, with --profile taking precedence.
                unsafe { std::env::set_var("NEWT_BUNDLE", b) };
            }
            // --loadout (NEWT_LOADOUT): resolve the named composition and feed each
            // axis's EXISTING selector — explicit --profile/--bundle/--num-ctx/
            // --persona override it (a loadout is a default you can poke). This is
            // the dispatcher-not-merger design: run_chat resolves each axis as usual.
            let loadout_role: Option<String> = {
                let name = cli
                    .loadout
                    .clone()
                    .or_else(|| std::env::var("NEWT_LOADOUT").ok())
                    .filter(|s| !s.is_empty());
                if let Some(name) = name {
                    let cfg = newt_core::Config::resolve()?;
                    let loadout = cfg.loadouts.get(&name).ok_or_else(|| {
                        let known = if cfg.loadouts.is_empty() {
                            "none defined".to_string()
                        } else {
                            cfg.loadouts.keys().cloned().collect::<Vec<_>>().join(", ")
                        };
                        anyhow::anyhow!("no such loadout '{name}' (known: {known})")
                    })?;
                    loadout
                        .validate(&cfg)
                        .map_err(|e| anyhow::anyhow!("loadout '{name}': {e}"))?;
                    // SAFETY: single-threaded before the TUI starts async work.
                    unsafe {
                        if cli.profile.is_none() {
                            if let Some(p) = &loadout.profile {
                                std::env::set_var("NEWT_PROFILE", p);
                            }
                        }
                        if cli.bundle.is_none() {
                            if let Some(k) = &loadout.kit {
                                std::env::set_var("NEWT_BUNDLE", k);
                            }
                        }
                        if cli.num_ctx.is_none() {
                            if let Some(n) = loadout.settings.as_ref().and_then(|s| s.num_ctx) {
                                std::env::set_var("NEWT_NUM_CTX", n.to_string());
                            }
                        }
                        // Provider axis (Slice 2): the loadout's `provider` names a
                        // [backends] entry; resolve_backend_choice honors NEWT_PROVIDER
                        // to select it (endpoint/kind/auth). Validated above.
                        if std::env::var_os("NEWT_PROVIDER").is_none() {
                            if let Some(p) = &loadout.provider {
                                std::env::set_var("NEWT_PROVIDER", p);
                            }
                        }
                        // Model selection: the catalog resolves `@variant` (catalog
                        // epic); here the bare model id feeds the existing selector and
                        // overrides the chosen backend's default model.
                        if std::env::var_os("NEWT_DGX_MODEL").is_none() {
                            if let Some(m) = &loadout.model {
                                let bare = m.split('@').next().unwrap_or(m);
                                std::env::set_var("NEWT_DGX_MODEL", bare);
                            }
                        }
                    }
                    // role → persona, unless --persona was given explicitly.
                    cli.persona
                        .as_ref()
                        .map_or_else(|| loadout.role.clone(), |_| None)
                } else {
                    None
                }
            };
            // --ephemeral threads to the TUI the same way (Step 17.7): the
            // session start resolution reads NEWT_EPHEMERAL once.
            if cli.ephemeral {
                unsafe { std::env::set_var("NEWT_EPHEMERAL", "1") };
            }
            // --prompt-for-permissions threads the same way (issue #263);
            // only the interactive TUI reads it — worker/eval never prompt.
            if cli.prompt_for_permissions {
                unsafe { std::env::set_var("NEWT_PROMPT_FOR_PERMISSIONS", "1") };
            }
            // --plain (alias --no-footer) forces the footer off; the TUI reads
            // NEWT_FOOTER once. CLI > env > [tui] footer > default (auto).
            if cli.plain {
                unsafe { std::env::set_var("NEWT_FOOTER", "off") };
            }
            // INTERIM (#297): --disable-ocap / --yolo threads the same way.
            // The run_command dispatch in newt-core reads NEWT_DISABLE_OCAP
            // per call; the TUI reads it once at session start for the loud
            // banner + the permission-log session record. Env-only on
            // purpose — no config key, no silent persistence.
            if cli.disable_ocap {
                unsafe { std::env::set_var("NEWT_DISABLE_OCAP", "1") };
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
            // The loadout's `role` is the persona when `--persona` was not given.
            let persona = cli.persona.as_deref().or(loadout_role.as_deref());
            // #479 part 2 — the crew/team runner: BUILT HERE (newt-cli owns
            // newt-scheduler + the worktree) and injected DOWN into the
            // scheduler-free TUI loop. Enabled by NEWT_TEAM (the operator's /team
            // toggle); off by default, so nothing changes unless asked. Crews run
            // under attenuated caveats (the runner fails closed on a read-only
            // session) in an isolated worktree.
            let team_runner = if std::env::var("NEWT_TEAM").is_ok() {
                let dir = path
                    .as_deref()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                newt_core::Config::resolve()
                    .ok()
                    .map(|cfg| crate::crew_runner::LocalCrewRunner::new(cfg, dir))
            } else {
                None
            };
            newt_tui::run_code(
                path.as_deref(),
                no_splash,
                persona,
                team_runner
                    .as_ref()
                    .map(|r| r as &dyn newt_core::agentic::CrewRunner),
            )
        }
        Command::Pilot { flight_id } => newt_tui::run_pilot(&flight_id),
        Command::Worker {
            coder,
            operator_key_path,
            allow_no_key,
        } => run_worker(coder, operator_key_path, allow_no_key).await,
        Command::Mcp => run_mcp().await,
        Command::Crew {
            task,
            edit,
            crew,
            dir,
            test,
            max_attempts,
            dry_run,
        } => {
            if edit {
                // Edit-settings mode: the name comes from --crew, else the
                // positional slot. No task is run.
                let name = crew.or(task);
                return newt_tui::run_crew_edit(name.as_deref(), newt_tui::color_supported());
            }
            let task = task.ok_or_else(|| {
                anyhow::anyhow!("a task is required (or pass --edit to edit crew settings)")
            })?;
            let code = crew::run_cli(crew::CrewArgs {
                task,
                crew,
                dir,
                test,
                max_attempts,
                dry_run,
            })
            .await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Doctor => doctor::run(cli.config.as_deref()).await,
        Command::Config => config_cmd::run(cli.config.as_deref()),
        Command::Identity => identity_cmd::run(cli.config.as_deref()),
        Command::Init => newt_tui::run_init(newt_tui::color_supported()),
        Command::Setup => newt_tui::run_setup(newt_tui::color_supported()),
        Command::Auth { server } => auth_cmd::run(server),
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
    fn parses_prompt_for_permissions_global() {
        // #263: works bare (default `code` command) and explicit; OFF by
        // default — no flag means denial behavior is unchanged.
        let cli = Cli::try_parse_from(["newt", "--prompt-for-permissions"]).unwrap();
        assert!(cli.prompt_for_permissions);
        let cli = Cli::try_parse_from(["newt", "code", "--prompt-for-permissions"]).unwrap();
        assert!(cli.prompt_for_permissions);
        let cli = Cli::try_parse_from(["newt"]).unwrap();
        assert!(!cli.prompt_for_permissions);
    }

    #[test]
    fn parses_disable_ocap_and_yolo_alias() {
        // #297: works bare (default `code` command) and explicit, and the
        // --yolo alias maps to the same field; OFF by default — no flag
        // means the confined dispatch is unchanged.
        let cli = Cli::try_parse_from(["newt", "--disable-ocap"]).unwrap();
        assert!(cli.disable_ocap);
        let cli = Cli::try_parse_from(["newt", "--yolo"]).unwrap();
        assert!(cli.disable_ocap);
        let cli = Cli::try_parse_from(["newt", "code", "--yolo"]).unwrap();
        assert!(cli.disable_ocap);
        assert!(matches!(cli.command, Some(Command::Code { .. })));
        let cli = Cli::try_parse_from(["newt"]).unwrap();
        assert!(!cli.disable_ocap);
    }

    #[test]
    fn parses_repeated_read_and_write_grants() {
        let cli = Cli::try_parse_from([
            "newt",
            "--read",
            "/a/.newt",
            "--read",
            "/a/x.yml",
            "--write",
            "/a/scratch",
        ])
        .unwrap();
        assert_eq!(
            cli.read_paths,
            vec![PathBuf::from("/a/.newt"), PathBuf::from("/a/x.yml")]
        );
        assert_eq!(cli.write_paths, vec![PathBuf::from("/a/scratch")]);
    }

    #[test]
    fn abs_grant_path_expands_tilde_and_absolutises() {
        use std::path::Path;
        // A leading ~ expands to the home dir. Use a platform-absolute HOME so
        // the expansion is itself absolute on both Unix and Windows (a bare
        // `/home/u` is NOT absolute on Windows, where it would be re-based).
        let home = if cfg!(windows) {
            r"C:\home\u"
        } else {
            "/home/u"
        };
        std::env::set_var("HOME", home);
        assert_eq!(
            abs_grant_path(Path::new("~/.newt")),
            PathBuf::from(home).join(".newt")
        );
        // An already-absolute path is unchanged.
        let abs = if cfg!(windows) {
            r"C:\etc\hosts"
        } else {
            "/etc/hosts"
        };
        assert_eq!(abs_grant_path(Path::new(abs)), PathBuf::from(abs));
        // A relative path is joined onto the current dir.
        let rel = abs_grant_path(Path::new("sub/file"));
        assert!(rel.is_absolute(), "relative grant absolutised: {rel:?}");
        assert!(rel.ends_with("sub/file"));
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
