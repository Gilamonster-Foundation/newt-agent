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
mod compaction_cmd;
mod config_cmd;
pub mod crew;
pub mod crew_runner;
mod dgx;
mod dgx_card;
mod dgx_pull;
pub mod dgx_registry;
pub mod dgx_status;
pub mod dgx_vllm;
mod doctor;
pub mod help_suite;
mod identity_cmd;
mod mcp_cmd;
mod mcp_probe_cmd;
mod models_cmd;
mod new_project;
mod ocap_cmd;
mod skills;
mod solve;
pub mod stack;
pub mod stdio_guard;
mod summarizer_cmd;
mod tuning_cmd;

use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;

/// clap value parser for `--shell-engine`. Delegates to [`newt_core::ShellEngine`]'s
/// `FromStr` (canonical names plus aliases like `landlock`→`host`); a bad value
/// is rejected at parse time with the list of accepted engines.
fn parse_shell_engine(
    s: &str,
) -> Result<newt_core::ShellEngine, Box<dyn std::error::Error + Send + Sync + 'static>> {
    Ok(s.parse::<newt_core::ShellEngine>()?)
}

/// Build a parser command graph with the shared `help` formatting applied to
/// this command and all subcommands.
pub fn help_command() -> clap::Command {
    help_suite::help_command()
}

/// Parse CLI args with the shared help format wiring.
pub fn parse_with_help() -> anyhow::Result<Cli> {
    help_suite::parse_with_help()
}

#[derive(Parser, Debug)]
#[command(name = "newt", version, about = "Free, friendly, local agentic coder")]
// `newt help [command]` renders newt's INTERACTIVE `/help` command catalog
// startup-free (no session, no backend) via the `Help` subcommand below —
// deliberately taking over the name from clap's auto-generated `help`
// subcommand, whose CLI-tree text remains available as `newt --help`.
#[command(disable_help_subcommand = true)]
pub struct Cli {
    /// Path to config file (overrides default search order).
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    /// Use a different user config root instead of ~/.newt.
    /// Reads config.toml and sibling state files from this directory. An
    /// explicit --config file still wins for the main config document.
    #[arg(long, global = true, value_name = "DIR")]
    pub config_dir: Option<PathBuf>,

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

    /// Lean / flight / wyvern mode (issue #527): drop the rich footer and use the
    /// dead-simple LeanTUI text box, where each prompt renders as a timestamped
    /// server-log line (`[ts] ❯ <prompt>`). Equivalent to `NEWT_FOOTER=off` /
    /// `[tui] footer = "off"`. By default the rich footer shows on a TTY and
    /// auto-degrades to this lean morphology off one (pipes, `newt worker`).
    /// `-n` / `--neat` / `--lite` (vi's "no-swap" spirit) are the same switch.
    #[arg(
        short = 'n',
        long,
        visible_aliases = ["neat", "lite", "lean", "flight", "no-footer"],
        global = true,
        default_value_t = false
    )]
    pub plain: bool,

    /// Color / theme (issue #527): always|never|auto|minimal|inverted|dark|
    /// light|mono. Controls whether — and how — ANSI color is emitted.
    /// Precedence: this flag > NO_COLOR / TERM=dumb > `[tui] color` > auto. An
    /// explicit `--color` also overrides NO_COLOR. Equivalent to `NEWT_COLOR`.
    #[arg(long, global = true, value_name = "MODE", value_parser = parse_color_mode)]
    pub color: Option<newt_core::ColorMode>,

    /// Force monochrome output (no color). Sugar for `--color=mono`; wins over
    /// `--color` when both are given.
    #[arg(long, global = true, default_value_t = false)]
    pub mono: bool,

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

    /// Operating altitude (FR-5, #999): `doer` acts (make the change), `coach`
    /// (alias `advise`) advises without mutating — it REPLACES the base identity
    /// with the coach soul. Overrides a `--persona`'s own altitude; with no
    /// `--persona` it runs a coach with no other role overlay.
    #[arg(long, global = true, value_name = "LEVEL", value_parser = ["doer", "coach", "advise"])]
    pub altitude: Option<String>,

    /// Tenacity (#tenacity): how hard the harness pushes the model from reading
    /// to ACTING — `relaxed` | `standard` | `insistent` | `relentless`. Higher
    /// forces an edit sooner (nudge after 6/3/2/1 read-only rounds) and makes
    /// plan-mode exit hand off to a mandatory edit. Default `standard`
    /// (behaviour-preserving). Small models that over-explore benefit from
    /// `insistent`/`relentless`.
    #[arg(long, global = true, value_name = "LEVEL", value_parser = parse_tenacity)]
    pub tenacity: Option<newt_core::Tenacity>,

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
    /// As of #721 this is now the DEFAULT for interactive sessions, so the
    /// flag is usually redundant; use `--no-prompt-for-permissions` to opt out.
    #[arg(long, global = true, default_value_t = false)]
    pub prompt_for_permissions: bool,

    /// Opt OUT of interactive permission prompting (#721). By default an
    /// interactive session now asks the operator on a capability denial; pass
    /// this to keep the plain, fail-closed denial instead (the model still gets
    /// the recoverable `request_permissions` guidance). Wins over
    /// `--prompt-for-permissions` / `[tui.permissions] prompt`. Equivalent to
    /// `NEWT_NO_PROMPT_FOR_PERMISSIONS=1`. Headless runs are unaffected (they
    /// never prompt regardless).
    #[arg(long, global = true, default_value_t = false)]
    pub no_prompt_for_permissions: bool,

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

    /// Override the configured `[tui.permissions]` preset with `full_access`
    /// for THIS invocation — session authority becomes unrestricted (fs fence,
    /// net leash, and exec allowlist all lifted; `write_file` behaves exactly
    /// as it does under the `full_access` preset). A DISTINCT switch from
    /// `--disable-ocap`/`--yolo`: this widens *authority*, while `--yolo`
    /// changes the exec *mechanism* (host shell vs confined shell) and still
    /// honors the active exec floor. Combine them (`--yolo --full-access`)
    /// for a fully unrestricted host shell. Equivalent to `NEWT_FULL_ACCESS=1`;
    /// deliberately NO config-file key beyond the existing preset, so the
    /// per-run override can never silently persist.
    #[arg(long, global = true, default_value_t = false)]
    pub full_access: bool,

    /// Select the shell **engine** `run_command` uses for THIS invocation (the
    /// ADR 0005 D2 seam): `safe-subset` (portable default — refuses
    /// `$(...)`/dynamic constructs), `host` (real `/bin/sh -c` inside the L3
    /// kernel jail — full grammar; what `--full-access` auto-selects), or
    /// `brush` (the carried bash-in-Rust engine + L2 interceptor; falls back to
    /// `host` until the brush build ships, agent-bridle#20). Overrides the
    /// `[shell] engine` config key. The L3 backend (Landlock/Seatbelt) is a
    /// separate, auto-selected axis.
    #[arg(long, global = true, value_parser = parse_shell_engine)]
    pub shell_engine: Option<newt_core::ShellEngine>,

    /// facade P4 (#780): turn OFF the convenience tool-call ROUTING for THIS
    /// invocation. By default a model's `run_command("cat X")` / `ls` / `find` /
    /// read-only `git` is silently rewritten to the governed built-in
    /// (`read_file`/`list_dir`/`find`/the git read path); `--no-route` runs the
    /// command on the normal exec path as-is instead. This is the L2
    /// convenience-OFF switch and is DELIBERATELY DISTINCT from `--disable-ocap`
    /// /`--yolo`: it NEVER disables the L3 boundary — the confined shell still
    /// gates exec and the fs fence still governs reads. Equivalent to
    /// `NEWT_NO_ROUTE=1`; env-only, no config key, so it cannot silently persist.
    #[arg(long, global = true, default_value_t = false)]
    pub no_route: bool,

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

    /// Backend override flags (`--backend-*`): pin the model backend from the
    /// command line, bypassing discovery and probe drop-ins.
    #[command(flatten)]
    pub backend: BackendArgs,

    /// Subcommand to run. Defaults to `code` (TUI coder) when omitted.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// `--backend-*` flags — set any backend field from the command line. Each
/// mirrors an operator-settable [`newt_core::config::BackendConfig`] field. When
/// an endpoint (or `--backend-model-path`) is given the flags define the sole,
/// exclusive backend for the invocation, so no discovery or probe drop-in can
/// reroute the session; otherwise the set fields override the selected backend
/// in place (e.g. just `--backend-model`). Built into a
/// [`newt_core::config::BackendOverride`] by [`Cli::backend_override`].
#[derive(clap::Args, Debug, Default, Clone)]
pub struct BackendArgs {
    /// Backend endpoint URL. Setting this makes the CLI backend EXCLUSIVE:
    /// discovery and probe drop-ins are bypassed for the session.
    #[arg(long = "backend-endpoint", global = true, value_name = "URL")]
    pub endpoint: Option<String>,

    /// Model this backend serves (e.g. `qwen3-coder_30b`).
    #[arg(long = "backend-model", global = true, value_name = "MODEL")]
    pub model: Option<String>,

    /// For `--backend-kind embedded`: local GGUF model file path.
    #[arg(long = "backend-model-path", global = true, value_name = "PATH")]
    pub model_path: Option<String>,

    /// Tiers this backend serves, comma-separated: FAST,STANDARD,COMPLEX,REVIEW
    /// (case-insensitive). Default when creating an exclusive backend: all four.
    #[arg(
        long = "backend-tiers",
        global = true,
        value_name = "TIERS",
        value_delimiter = ',',
        value_parser = parse_tier
    )]
    pub tiers: Vec<newt_core::Tier>,

    /// Wire protocol: `ollama`, `openai` (alias `vllm`), or `embedded`.
    #[arg(long = "backend-kind", global = true, value_name = "KIND", value_parser = parse_backend_kind)]
    pub kind: Option<newt_core::config::BackendKind>,

    /// OpenAI HTTP surface: `chat_completions` or `responses`.
    #[arg(long = "backend-api", global = true, value_name = "API", value_parser = parse_openai_api)]
    pub api: Option<newt_core::config::OpenAiApi>,

    /// Env var holding the bearer token (takes precedence over the file).
    #[arg(long = "backend-api-key-env", global = true, value_name = "VAR")]
    pub api_key_env: Option<String>,

    /// File whose first non-empty line is the bearer token.
    #[arg(long = "backend-api-key-file", global = true, value_name = "PATH")]
    pub api_key_file: Option<String>,

    /// Serving axis: `multiplexer` or `instance`.
    #[arg(long = "backend-serving", global = true, value_name = "AXIS", value_parser = parse_serving)]
    pub serving: Option<newt_core::config::Serving>,

    /// Physical host of the endpoint, for same-host reasoning.
    #[arg(long = "backend-host", global = true, value_name = "HOST")]
    pub host: Option<String>,

    /// Assert this host can run this backend alongside others (suppress the
    /// same-host starvation rule).
    #[arg(long = "backend-coexist", global = true, value_name = "BOOL")]
    pub coexist: Option<bool>,

    /// Host memory available for serving (GiB), for the crew fit-gate.
    #[arg(long = "backend-ram-gib", global = true, value_name = "GIB")]
    pub ram_gib: Option<f64>,

    /// Model-card pointer for this backend.
    #[arg(long = "backend-card", global = true, value_name = "CARD")]
    pub card: Option<String>,

    /// Backend name (default `cli`). Names the exclusive backend, or selects
    /// which existing backend a field-only override targets.
    #[arg(long = "backend-name", global = true, value_name = "NAME")]
    pub name: Option<String>,
}

impl BackendArgs {
    /// Build the [`newt_core::config::BackendOverride`] these flags describe.
    /// Unset flags stay `None`; an empty `--backend-tiers` stays `None` (not an
    /// empty tier list), so a field-only override never accidentally clears
    /// tiers.
    pub fn to_override(&self) -> newt_core::config::BackendOverride {
        newt_core::config::BackendOverride {
            name: self.name.clone(),
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            model_path: self.model_path.clone(),
            tiers: (!self.tiers.is_empty()).then(|| self.tiers.clone()),
            kind: self.kind,
            api: self.api,
            api_key_env: self.api_key_env.clone(),
            api_key_file: self.api_key_file.clone(),
            serving: self.serving,
            host: self.host.clone(),
            coexist: self.coexist,
            ram_gib: self.ram_gib,
            card: self.card.clone(),
        }
    }
}

fn parse_tier(s: &str) -> Result<newt_core::Tier, String> {
    use newt_core::Tier;
    match s.trim().to_ascii_uppercase().as_str() {
        "FAST" => Ok(Tier::Fast),
        "STANDARD" => Ok(Tier::Standard),
        "COMPLEX" => Ok(Tier::Complex),
        "REVIEW" => Ok(Tier::Review),
        _ => Err(format!("unknown tier '{s}' (FAST|STANDARD|COMPLEX|REVIEW)")),
    }
}

fn parse_tenacity(s: &str) -> Result<newt_core::Tenacity, String> {
    s.parse()
}

fn parse_backend_kind(s: &str) -> Result<newt_core::config::BackendKind, String> {
    use newt_core::config::BackendKind;
    match s.trim().to_ascii_lowercase().as_str() {
        "ollama" => Ok(BackendKind::Ollama),
        "openai" | "vllm" | "openai-compatible" => Ok(BackendKind::Openai),
        "embedded" => Ok(BackendKind::Embedded),
        _ => Err(format!(
            "unknown backend kind '{s}' (ollama|openai|embedded)"
        )),
    }
}

fn parse_openai_api(s: &str) -> Result<newt_core::config::OpenAiApi, String> {
    use newt_core::config::OpenAiApi;
    match s.trim().to_ascii_lowercase().as_str() {
        "chat_completions" | "chat-completions" | "chat" => Ok(OpenAiApi::ChatCompletions),
        "responses" => Ok(OpenAiApi::Responses),
        _ => Err(format!("unknown api '{s}' (chat_completions|responses)")),
    }
}

fn parse_serving(s: &str) -> Result<newt_core::config::Serving, String> {
    use newt_core::config::Serving;
    match s.trim().to_ascii_lowercase().as_str() {
        "multiplexer" | "mux" => Ok(Serving::Multiplexer),
        "instance" => Ok(Serving::Instance),
        _ => Err(format!("unknown serving '{s}' (multiplexer|instance)")),
    }
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Standalone TUI coder.
    Code {
        /// Optional working path.
        path: Option<PathBuf>,
    },
    /// Print newt's interactive command catalog without starting a session.
    ///
    /// `newt help` lists every `/command`; `newt help <command>` shows one
    /// command's detail page (the same text as the in-session `/help` and
    /// `/<command> --help`). Renders with no backend connect, so it works
    /// offline and in CI. For the CLI subcommand tree instead, use `newt
    /// --help`.
    Help {
        /// Optional `/command` name to show the detail page for (omit for the
        /// full list). A leading slash is accepted and ignored.
        command: Option<String>,
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
    /// Manage MCP servers, or run newt as one. Subcommands manage the
    /// `[[mcp_servers]]` registrations (`add` / `remove` / `list` /
    /// `install` / `probe`); `serve` runs newt as an MCP server over stdio.
    ///
    /// Bare `newt mcp` is TTY-aware: with piped stdin (an MCP client) it
    /// serves over stdio as before; at an interactive terminal it prints
    /// this subcommand menu instead of blocking as a server. Use `newt mcp
    /// serve` to serve unconditionally.
    Mcp {
        #[command(subcommand)]
        cmd: Option<mcp_cmd::McpCmd>,
    },
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
    /// AUTHOR a plan from a goal (`--goal`), or PREVIEW / `--execute` an existing
    /// plan TOML file leaf-by-leaf via a crew. Authoring asks a strong model to
    /// decompose the goal into a `plan::Plan` (default-deny caveats) for you to
    /// review/edit — it never executes. Execution is preview-by-default; `--execute`
    /// runs an autonomous DAG of crews with no per-leaf review (bounded by
    /// `--max-leaves`), writing a sibling `<file>.run.toml` (source untouched).
    Plan {
        /// The plan TOML file to preview/execute: a `[[subtask]]` list, optionally
        /// a tree (`parent`) + a DAG (`deps`). Omit when authoring with `--goal`.
        file: Option<PathBuf>,
        /// Author a plan FROM this goal (a strong model decomposes it) instead of
        /// reading a file; writes to `--output` (or stdout). Never executes.
        #[arg(long, conflicts_with = "file")]
        goal: Option<String>,
        /// Where to write the authored plan (with `--goal`). Default: stdout.
        #[arg(long, short = 'o', value_name = "FILE")]
        output: Option<PathBuf>,
        /// Target repo dir (default: current dir). Must be a git repo.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Actually dispatch the crews. Without this, `newt plan` only PREVIEWS —
        /// autonomous multi-crew execution needs this explicit second affirmation.
        #[arg(long, default_value_t = false)]
        execute: bool,
        /// One-shot: AUTHOR (with `--goal`) **and** execute the plan autonomously
        /// in a single gesture, or one-shot an existing plan FILE end-to-end. Like
        /// `--execute`, the flag IS the approval — the plan runs with no per-leaf
        /// review (bounded by `--max-leaves`). The headless autonomous drive (e.g.
        /// the #548 evaluator): `newt plan --goal "…" --one-shot`.
        #[arg(long = "one-shot", default_value_t = false)]
        one_shot: bool,
        /// Cap on leaves to execute (and subtasks to author) without an explicit
        /// raise — each leaf is an autonomous crew with no per-leaf review.
        #[arg(long, default_value_t = 8)]
        max_leaves: usize,
        /// LOCKED behavioral gate (structurally-enforced TDD): an
        /// operator-supplied verify command that OVERRIDES every leaf's gate.
        /// Unlike a model-authored `verify`, it is trusted like the repo-inferred
        /// command (it comes from this human flag, not the model), so a crew
        /// cannot pass by deleting or weakening its own test. Typically a
        /// "restore the immutable spec, then run it" command. Only meaningful
        /// with `--one-shot`.
        #[arg(long = "locked-verify")]
        locked_verify: Option<String>,
    },
    /// Health-check local backends + provider plugins.
    Doctor {
        /// #1207: bless ~/.newt/ocap/approve.toml — sign every entry with your
        /// root key so it loads as a valid durable grant (unsigned entries drop
        /// fail-closed at session start). Running this IS the authorization:
        /// you are vouching for the file as it stands. High-danger targets are
        /// refused and reported (exit 2).
        #[arg(long = "sign-ocap")]
        sign_ocap: bool,
    },
    /// Manage the `~/.newt/ocap/` durable-policy store. `propose` folds the
    /// flight-recorder capture of a `--full-access` session into reviewable,
    /// unsigned `approve.toml` candidates (bless them with
    /// `newt doctor --sign-ocap`).
    Ocap {
        #[command(subcommand)]
        cmd: ocap_cmd::OcapCmd,
    },
    /// Solve one task HEADLESS and emit a trace (Terminal-Bench / #1419). Drives
    /// the same agentic loop the TUI runs, non-interactively, and exits. Reads
    /// the task from `--instruction-file`, runs in `--cwd`, and appends a JSONL
    /// trace to `--events`. `--non-interactive` (the default here) runs OCAP-off
    /// + full-access with no prompts — the benchmark bootstrap lane.
    Solve {
        /// Workspace directory the agent runs against (default: current dir).
        #[arg(long, value_name = "DIR")]
        cwd: Option<PathBuf>,
        /// File whose contents are the task instruction.
        #[arg(long, value_name = "FILE")]
        instruction_file: PathBuf,
        /// Run OCAP-off + full-access with no prompts (default true).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        non_interactive: bool,
        /// Append a JSONL trace record here.
        #[arg(long, value_name = "FILE")]
        events: Option<PathBuf>,
        /// Override the max tool-call rounds for this solve.
        #[arg(long, value_name = "N")]
        max_rounds: Option<usize>,
        /// The served model's context window (input-token ceiling), so
        /// compaction keeps each request under the backend's `--ctx-size` (e.g.
        /// 32768) instead of overrunning it.
        #[arg(long, value_name = "N")]
        context_window: Option<usize>,
    },
    /// Print resolved config.
    Config,
    /// Diagnose + tune mid-loop context compaction (the summarizer): the
    /// effective trim trigger, count-vs-token firing, the summarizer backend,
    /// and warnings for over-aggressive firing / the no-abort hang (#979).
    Compaction,
    /// Show or set the agent commit identity (`.newt/agent-identity.toml`).
    ///
    /// Bare `newt identity` prints name, email, source layer, trailer,
    /// signing-key path + fingerprint, GitHub App coordinates, and token
    /// NAMES (never secret values). `newt identity set --name … --email …`
    /// writes an override file — the same path a future setup dialog will use.
    /// Default (no file): GitHub User https://github.com/newt-agent.
    Identity {
        #[command(subcommand)]
        cmd: Option<identity_cmd::IdentityCmd>,
    },
    /// Run (or re-run) the setup wizard: probe Ollama + write ~/.newt/config.toml.
    /// Edit that file directly for everything else — newt has no settings UI.
    Init,
    /// Configure an inference backend from a target, or run the interactive
    /// first-run wizard when no target is supplied.
    Setup {
        /// Backend hostname or URL to probe. Bare hosts expand through the
        /// configured discovery ports; URLs and host:port targets are singular.
        target: Option<String>,

        /// Environment variable containing the backend bearer token. Requires
        /// an explicit HTTPS URL (HTTP is allowed only for loopback).
        #[arg(
            long,
            visible_alias = "api-key-env",
            conflicts_with = "token_file",
            requires = "target"
        )]
        token_env: Option<String>,

        /// File containing the backend bearer token. Requires an explicit HTTPS
        /// URL (HTTP only for loopback); relative paths become absolute.
        #[arg(
            long,
            visible_alias = "api-key-file",
            conflicts_with = "token_env",
            requires = "target"
        )]
        token_file: Option<PathBuf>,

        /// Write the detected backend configuration without confirmation.
        #[arg(long, short = 'y', requires = "target")]
        yes: bool,
    },
    /// Scaffold a NEW project for an ecosystem, already wired for its lifecycle
    /// phases. `newt new pyo3 mypkg` lays down a minimal, buildable Rust+PyO3
    /// (maturin) project; `python` and `rust` too. Templates are DATA (built-in
    /// or `~/.newt/templates/<name>.toml` drop-ins). With no ecosystem, lists the
    /// available templates.
    New {
        /// Ecosystem template: `pyo3` | `python` | `rust` (or a drop-in name).
        /// Omit to list available templates.
        ecosystem: Option<String>,
        /// Project name (default: the target directory's final component).
        name: Option<String>,
        /// Target directory (default: `./<name>`).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Write into a non-empty target directory (overwriting colliding files).
        #[arg(long, default_value_t = false)]
        force: bool,
    },
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
    /// Manage the on-host mini-model palette for the embedded CPU summarizer
    /// (#661): `pull` a GGUF to ~/.newt/models, `list` the palette, `path`.
    Models {
        #[command(subcommand)]
        cmd: models_cmd::ModelsCmd,
    },
    /// Inspect and configure the mid-loop summarizer backend.
    Summarizer {
        #[command(subcommand)]
        cmd: Option<summarizer_cmd::SummarizerCmd>,
    },
}

/// clap `value_parser` for `--color`: parse a keyword into a
/// [`newt_core::ColorMode`] (keeps newt-core clap-free — no `ValueEnum` derive).
fn parse_color_mode(s: &str) -> Result<newt_core::ColorMode, String> {
    newt_core::ColorMode::from_keyword(s).ok_or_else(|| {
        format!(
            "invalid color mode '{s}' (expected one of: \
             always, never, auto, minimal, inverted, dark, light, mono)"
        )
    })
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
    // #1303 clause B: install the one-time mouse-capture panic-release hook at
    // binary entry, before any turn can enable capture. It emits
    // `DisableMouseCapture` ONLY when capture is currently active, so the
    // `worker`/`mcp`/piped paths (which never enable it) stay byte-for-byte
    // clean (clause E). Compiled out of the wyvern/lean build.
    #[cfg(all(unix, any(feature = "rich-tui", feature = "live-spill")))]
    newt_tui::install_panic_release_hook();

    if let Some(dir) = cli.config_dir.as_deref() {
        let dir = abs_grant_path(dir);
        // SAFETY: single-threaded before any async work or config resolution.
        unsafe { std::env::set_var(newt_core::config::NEWT_CONFIG_DIR_ENV, dir) };
    }

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

    // --color / --mono (issue #527): thread the color mode to every surface via
    // NEWT_COLOR (global flags, so set before the command match). --mono wins
    // over --color. Resolution (flag > NO_COLOR/TERM=dumb > [tui] color > auto)
    // happens in the TUI color layer.
    let color_kw = if cli.mono {
        Some("mono")
    } else {
        cli.color.map(|c| c.keyword())
    };
    if let Some(kw) = color_kw {
        // SAFETY: single-threaded before the TUI starts any async work.
        unsafe { std::env::set_var("NEWT_COLOR", kw) };
    }

    // Denial repair journal: arm every agent surface, including headless
    // workers/crews, before command dispatch. The journal is evidence only and
    // is never read back into authority. An explicit `off`/`0` opts out.
    if std::env::var_os(newt_core::denial_journal::DENIAL_JOURNAL_PATH_ENV).is_none() {
        if let Some(cfg_path) = newt_core::Config::user_config_path() {
            let journal = cfg_path.with_file_name("denial-journal.jsonl");
            unsafe {
                std::env::set_var(newt_core::denial_journal::DENIAL_JOURNAL_PATH_ENV, journal);
            }
        }
    }
    if let Ok(v) = std::env::var(newt_core::denial_journal::DENIAL_JOURNAL_PATH_ENV) {
        if v.eq_ignore_ascii_case("off") || v == "0" {
            unsafe {
                std::env::remove_var(newt_core::denial_journal::DENIAL_JOURNAL_PATH_ENV);
            }
        }
    }

    // CLI `--backend-*` flags: install the process-global override so every
    // Config::resolve honors it, and — when a destination is pinned — set
    // NEWT_PROVIDER to the backend so the tier→backend selector picks exactly
    // it. This is the explicit escape hatch against discovery/probe drop-ins
    // silently rerouting the session (the local-ollama-fallback incident).
    {
        let over = cli.backend.to_override();
        if !over.is_empty() {
            let has_destination = over.endpoint.is_some() || over.model_path.is_some();
            let provider = over.name.clone().unwrap_or_else(|| "cli".to_string());
            newt_core::config::set_cli_backend_override(over);
            if has_destination {
                // SAFETY: single-threaded before the TUI starts any async work.
                unsafe { std::env::set_var("NEWT_PROVIDER", provider) };
            }
        }
    }

    // CLI `--tenacity`: install the process-global action-forcing level the
    // agentic loop reads when it builds each turn's WorkflowRuntimeState.
    if let Some(level) = cli.tenacity {
        newt_core::tenacity::set_cli_tenacity(level);
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
            // Sticky last-active selections (#545): the provider/model the user
            // last chose via `/backends`/`/model` are restored from
            // ~/.newt/settings.toml so the next start picks up where they left
            // off. LOWEST precedence — an explicit NEWT_PROVIDER/NEWT_DGX_MODEL
            // (env) or a --loadout axis (set just above) always wins, and a
            // provider naming a since-removed [[backends]] entry is ignored.
            {
                let session = newt_core::settings::load();
                if !session.is_empty() {
                    let cfg = newt_core::Config::resolve().ok();
                    let current_provider = std::env::var("NEWT_PROVIDER")
                        .ok()
                        .filter(|s| !s.is_empty());
                    let restore = session.restore(
                        current_provider.as_deref(),
                        std::env::var_os("NEWT_DGX_MODEL").is_some(),
                        |name| {
                            cfg.as_ref()
                                .is_some_and(|c| c.backends.iter().any(|b| b.name == name))
                        },
                    );
                    // SAFETY: single-threaded before the TUI starts async work.
                    unsafe {
                        if let Some(provider) = restore.provider {
                            std::env::set_var("NEWT_PROVIDER", provider);
                        }
                        if let Some(model) = restore.model {
                            std::env::set_var("NEWT_DGX_MODEL", model);
                        }
                    }
                }
            }
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
            // #721: --no-prompt-for-permissions opts back out of the new
            // interactive default; it wins over --prompt-for-permissions/config.
            if cli.no_prompt_for_permissions {
                unsafe { std::env::set_var("NEWT_NO_PROMPT_FOR_PERMISSIONS", "1") };
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
            // --full-access threads the same way: the TUI's policy_for reads
            // NEWT_FULL_ACCESS when building the session capability, and the
            // session banner surfaces it loudly. Env-only on purpose — the
            // per-run override never persists to config.
            if cli.full_access {
                unsafe { std::env::set_var("NEWT_FULL_ACCESS", "1") };
            }
            // #1176: arm the shadow-OCAP flight recorder whenever the session
            // runs UNCONFINED (--full-access or --yolo/--disable-ocap). Every
            // unconfined command then records the authority a leash would have
            // gated on, building a policy-gap catalog + bridle repro fixtures.
            // Respects an explicit NEWT_FLIGHT_RECORDER (a chosen path, or
            // `off`/`0` to opt out); default is ~/.newt/flight-recorder/
            // unconfined.jsonl (append-only across runs).
            if (cli.full_access || cli.disable_ocap)
                && std::env::var_os(newt_core::flight_recorder::CAPTURE_PATH_ENV).is_none()
            {
                if let Some(cfg_path) = newt_core::Config::user_config_path() {
                    let capture = cfg_path
                        .with_file_name("flight-recorder")
                        .join("unconfined.jsonl");
                    unsafe {
                        std::env::set_var(newt_core::flight_recorder::CAPTURE_PATH_ENV, &capture);
                    }
                }
            }
            // Explicit opt-out (`off`/`0`) clears the recorder for this run.
            if let Ok(v) = std::env::var(newt_core::flight_recorder::CAPTURE_PATH_ENV) {
                if v.eq_ignore_ascii_case("off") || v == "0" {
                    unsafe { std::env::remove_var(newt_core::flight_recorder::CAPTURE_PATH_ENV) };
                }
            }
            // --shell-engine selects which agent-bridle engine run_command uses
            // (ADR 0005 D2 seam). Resolved ONCE here — precedence
            // `--shell-engine` > `[shell] engine` > `--full-access`→`host` >
            // `safe-subset` — and published via NEWT_SHELL_ENGINE for newt-core's
            // dispatch to read (same env pattern as NEWT_FULL_ACCESS).
            {
                let shell_cfg = newt_core::Config::resolve().ok().and_then(|c| c.shell);
                // #1243 Leg 1: publish NEWT_SHELL_ENGINE only for a FIXED choice
                // (explicit flag/config or the --full-access auto-upgrade). The
                // confined default is intentionally left UNPUBLISHED so the deep
                // dispatch resolves it per-command against the live L3 fence
                // (confined_default_engine) — never caching the startup fence
                // state (the agent-bridle #239 TOCTOU obligation).
                if let Some(engine) = newt_core::resolve_shell_engine_choice(
                    cli.shell_engine,
                    shell_cfg.as_ref().and_then(|s| s.engine),
                    cli.full_access,
                ) {
                    unsafe { std::env::set_var("NEWT_SHELL_ENGINE", engine.as_str()) };
                }
                // Confined-shell env passthrough (so `~` expands — brush needs
                // HOME): `[shell] env_passthrough` (default HOME+USER), published
                // colon-separated for newt-core's confined dispatch to seed.
                let passthrough = shell_cfg
                    .and_then(|s| s.env_passthrough)
                    .unwrap_or_else(newt_core::shell_env_passthrough_default);
                unsafe {
                    std::env::set_var("NEWT_SHELL_ENV_PASSTHROUGH", passthrough.join(":"));
                }
            }
            // facade P4 (#780): --no-route turns OFF the convenience tool-call
            // routing (run_command reads the var per call). A DISTINCT switch
            // from --disable-ocap (§7-F5): L2-convenience-off, L3-boundary
            // untouched — the two env vars never alias. Env-only, no config key.
            if cli.no_route {
                unsafe { std::env::set_var("NEWT_NO_ROUTE", "1") };
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
                // The NEWT_TEAM enable IS the human gesture today — a soft
                // affirmation, modelled as `Presence::Prompt` (23.2). It's the
                // dev-escape stand-in for the real attest ceremony, which arrives
                // with BOOT (#472); a `Passkey`-required crew op holds until then.
                newt_core::Config::resolve().ok().map(|cfg| {
                    crate::crew_runner::LocalCrewRunner::new(
                        cfg,
                        dir,
                        newt_core::agentic::Presence::Prompt,
                    )
                })
            } else {
                None
            };
            // Best-effort: if the DGX node has drifted from [dgx] config, print a
            // one-line notice (→ `newt dgx adopt`) before the session. Never
            // blocks startup; silent when dgx is unconfigured or unreachable.
            dgx::startup_drift_notice(cli.config.as_deref()).await;
            // First-run provisioning is COVERED by the splash (#985): build a
            // background setup handle (interactive + embedded + unprovisioned) and
            // hand it to run_code, which shows a spinner over the download instead
            // of dumping raw output before the TUI (which let a stray key dismiss
            // the splash). Non-interactive / lean / already-present → None.
            let setup = models_cmd::spawn_setup();
            // FR-5 (#999): map the validated `--altitude` string to the enum.
            // `value_parser` already fenced the input to doer/coach/advise, so a
            // non-"coach"/"advise" value can only be "doer".
            let altitude = cli.altitude.as_deref().map(|level| match level {
                "coach" | "advise" => newt_core::Altitude::Coach,
                _ => newt_core::Altitude::Doer,
            });
            newt_tui::run_code(
                path.as_deref(),
                no_splash,
                persona,
                altitude,
                team_runner
                    .as_ref()
                    .map(|r| r as &dyn newt_core::agentic::CrewRunner),
                setup,
            )
        }
        Command::Pilot { flight_id } => newt_tui::run_pilot(&flight_id),
        Command::Worker {
            coder,
            operator_key_path,
            allow_no_key,
        } => run_worker(coder, operator_key_path, allow_no_key).await,
        // #1021 PR 5.3: `--persona` is already a global flag (parsed for
        // every subcommand); it was just silently dropped here before now.
        //
        // Bare `newt mcp` (no subcommand) is TTY-aware: piped stdin (an MCP
        // client, or the stdout-purity tests) serves over stdio exactly as
        // before; an interactive terminal prints the subcommand menu instead
        // of blocking as a server on a human's stdin. `newt mcp serve` always
        // serves regardless of TTY. See `mcp_cmd::bare_mcp_action`.
        Command::Mcp { cmd: None } => {
            match mcp_cmd::bare_mcp_action(std::io::stdin().is_terminal()) {
                mcp_cmd::BareMcpAction::Serve => run_mcp(cli.persona.as_deref()).await,
                mcp_cmd::BareMcpAction::Help => print_mcp_help(),
            }
        }
        Command::Mcp {
            cmd: Some(mcp_cmd::McpCmd::Serve),
        } => run_mcp(cli.persona.as_deref()).await,
        Command::Mcp { cmd: Some(cmd) } => mcp_cmd::run(cmd, cli.config.as_deref()).await,
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
        Command::Plan {
            file,
            goal,
            output,
            dir,
            execute,
            one_shot,
            max_leaves,
            locked_verify,
        } => {
            let code = if let Some(goal) = goal {
                if one_shot {
                    // One gesture: author the plan AND execute it autonomously.
                    crew::one_shot_goal_cli(&goal, dir, max_leaves, locked_verify).await?
                } else {
                    // Author a plan from the goal (a strong model decomposes it),
                    // grounded in the target repo (`--dir`, else cwd).
                    crew::author_plan_cli(goal, output, max_leaves, dir).await?
                }
            } else if let Some(file) = file {
                // `--one-shot` on a FILE is the approval to run it end-to-end
                // (equivalent to `--execute`).
                crew::run_plan_cli(crew::PlanArgs {
                    file,
                    dir,
                    execute: execute || one_shot,
                    one_shot,
                    max_leaves,
                })
                .await?
            } else {
                anyhow::bail!(
                    "pass a plan FILE to preview/execute, or --goal \"<text>\" to author one"
                );
            };
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Doctor { sign_ocap } => {
            if sign_ocap {
                let code = doctor::sign_ocap()?;
                if code != 0 {
                    std::process::exit(code);
                }
                Ok(())
            } else {
                doctor::run(cli.config.as_deref()).await
            }
        }
        Command::Ocap { cmd } => {
            let code = ocap_cmd::run(cmd, cli.config.as_deref())?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Solve {
            cwd,
            instruction_file,
            non_interactive,
            events,
            max_rounds,
            context_window,
        } => {
            let code = solve::run(solve::SolveArgs {
                cwd: cwd.unwrap_or_else(|| PathBuf::from(".")),
                instruction_file,
                // The pinned benchmark profile is the global `--config <FILE>`.
                profile: cli.config.clone(),
                non_interactive,
                events,
                max_rounds,
                context_window,
            })
            .await?;
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        Command::Config => config_cmd::run(cli.config.as_deref()),
        Command::Compaction => compaction_cmd::run(cli.config.as_deref()),
        Command::Identity { cmd } => identity_cmd::run(cli.config.as_deref(), cmd),
        Command::Init => newt_tui::run_init(newt_tui::color_supported()),
        Command::Setup {
            target,
            token_env,
            token_file,
            yes,
        } => match target {
            Some(target) => {
                newt_tui::run_setup_target(
                    &target,
                    token_env.as_deref(),
                    token_file.as_deref(),
                    yes,
                    cli.config.as_deref(),
                )
                .await
            }
            None => newt_tui::run_setup(newt_tui::color_supported()),
        },
        Command::Auth { server } => auth_cmd::run(server),
        Command::New {
            ecosystem,
            name,
            dir,
            force,
        } => new_project::run(ecosystem, name, dir, force),
        Command::Skills { cmd } => skills::run(cmd, cli.config.as_deref()),
        Command::Dgx { cmd } => dgx::run(cmd, cli.config.as_deref()).await,
        Command::Tunings { cmd } => tuning_cmd::run(cmd, cli.config.as_deref()),
        Command::Models { cmd } => models_cmd::run(cmd).await,
        Command::Summarizer { cmd } => summarizer_cmd::run(cmd).await,
        // Startup-free help: no session, no backend connect. Renders the SAME
        // bytes the interactive `/help` prints (both route through
        // `newt_tui::render_help`), so `newt help` is a hosted-CI-safe way to
        // inspect the command catalog. A leading `/` on the topic is tolerated
        // so `newt help /dgx` and `newt help dgx` behave alike.
        Command::Help { command } => {
            let topic = command.as_deref().map(|c| c.trim_start_matches('/'));
            print!(
                "{}",
                newt_tui::render_help(topic, newt_tui::color_supported(), false)
            );
            Ok(())
        }
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
    // fd 1 is the ACP JSON-RPC wire from here on. Declare it BEFORE any other
    // work so no ephemeral writer can ever paint a frame into a protocol
    // stream. Deliberately unconditional on platform: the `dup2` guard below is
    // `#[cfg(unix)]` and its non-unix arm returns `ErrorKind::Unsupported`, so
    // on Windows this flag is the ONLY thing protecting the wire.
    newt_core::tty::enter_protocol_mode();

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
///
/// `persona` (#1021 PR 5.3) is the already-global `--persona` flag, wired
/// into this subcommand for the first time: when set, the server loads that
/// role profile, connects its declared MCP servers (e.g. `modulex`), and
/// restricts the advertised `tools/list` to the persona's `tools:`
/// allow-list — the same enforcement the TUI applies, reused via
/// `newt_core::agentic::filter_advertised_tools`.
/// Print the `mcp` subcommand menu for an interactive human who typed bare
/// `newt mcp` at a terminal — instead of blocking as a stdio server on their
/// keyboard. The verb list is rendered from clap's own help for the `mcp`
/// subcommand so it can never drift from the parser, followed by a one-line
/// pointer to the two ways to actually serve. Returns `Ok` (exit 0).
fn print_mcp_help() -> anyhow::Result<()> {
    let mut cmd = help_command();
    if let Some(mcp) = cmd.find_subcommand_mut("mcp") {
        // `render_long_help` lists each verb with its description — the menu.
        print!("{}", mcp.render_long_help());
    }
    println!(
        "\nTo run newt as an MCP server for a client, invoke it with piped \
         stdio (e.g. `claude mcp add newt -- newt mcp`) or run `newt mcp serve`."
    );
    Ok(())
}

async fn run_mcp(persona: Option<&str>) -> anyhow::Result<()> {
    // fd 1 is the MCP JSON-RPC wire from here on — see the note in
    // `run_worker`. Unconditional on platform, and irreversible.
    newt_core::tty::enter_protocol_mode();

    #[cfg(unix)]
    {
        match stdio_guard::redirect_stdout_to_stderr() {
            Ok(private_stdout) => {
                let tokio_stdout = tokio::fs::File::from_std(private_stdout);
                newt_mcp_server::run_with_io(tokio::io::stdin(), tokio_stdout, persona).await
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "stdio_guard fd redirect failed; falling back to raw stdout"
                );
                newt_mcp_server::run_stdio(persona).await
            }
        }
    }
    #[cfg(not(unix))]
    {
        newt_mcp_server::run_stdio(persona).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn backend_flags_parse_and_build_an_exclusive_override() {
        let cli = Cli::try_parse_from([
            "newt",
            "--backend-endpoint",
            "http://router:8080",
            "--backend-model",
            "qwen3-coder_30b",
            "--backend-kind",
            "openai",
            "--backend-api",
            "chat_completions",
            "--backend-tiers",
            "FAST,STANDARD,COMPLEX,REVIEW",
            "--backend-api-key-file",
            "/vault/token",
        ])
        .unwrap();
        let over = cli.backend.to_override();
        assert!(!over.is_empty());
        assert_eq!(over.endpoint.as_deref(), Some("http://router:8080"));
        assert_eq!(over.model.as_deref(), Some("qwen3-coder_30b"));
        assert_eq!(over.kind, Some(newt_core::config::BackendKind::Openai));
        assert_eq!(
            over.api,
            Some(newt_core::config::OpenAiApi::ChatCompletions)
        );
        assert_eq!(over.api_key_file.as_deref(), Some("/vault/token"));
        assert_eq!(
            over.tiers,
            Some(vec![
                newt_core::Tier::Fast,
                newt_core::Tier::Standard,
                newt_core::Tier::Complex,
                newt_core::Tier::Review,
            ])
        );

        // The override is exclusive and replaces discovered backends.
        let mut cfg = newt_core::Config {
            backends: vec![newt_core::config::BackendConfig {
                name: "discovered".into(),
                endpoint: "http://localhost:11434".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        over.apply(&mut cfg);
        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(cfg.backends[0].name, "cli");
        assert_eq!(cfg.backends[0].endpoint, "http://router:8080");
    }

    #[test]
    fn no_backend_flags_yields_an_empty_override() {
        let cli = Cli::try_parse_from(["newt"]).unwrap();
        assert!(cli.backend.to_override().is_empty());
    }

    #[test]
    fn backend_kind_rejects_garbage() {
        assert!(Cli::try_parse_from(["newt", "--backend-kind", "banana"]).is_err());
    }

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

    // ── color / theme flags (issue #527) ────────────────────────────────

    #[test]
    fn parses_color_mode_global() {
        // Works bare (default `code`) and after the subcommand; off by default.
        let cli = Cli::try_parse_from(["newt", "--color", "always"]).unwrap();
        assert_eq!(cli.color, Some(newt_core::ColorMode::Always));
        let cli = Cli::try_parse_from(["newt", "code", "--color", "dark"]).unwrap();
        assert_eq!(cli.color, Some(newt_core::ColorMode::Dark));
        let cli = Cli::try_parse_from(["newt"]).unwrap();
        assert_eq!(cli.color, None);
    }

    #[test]
    fn rejects_unknown_color_mode() {
        // The value_parser surfaces a clap error for an unknown keyword.
        assert!(Cli::try_parse_from(["newt", "--color", "rainbow"]).is_err());
    }

    #[test]
    fn parses_mono_global() {
        let cli = Cli::try_parse_from(["newt", "--mono"]).unwrap();
        assert!(cli.mono);
        let cli = Cli::try_parse_from(["newt"]).unwrap();
        assert!(!cli.mono);
    }

    #[test]
    fn mono_and_color_can_coexist_mono_wins_in_dispatch() {
        // Both parse; dispatch resolves --mono ahead of --color (mono wins).
        let cli = Cli::try_parse_from(["newt", "--mono", "--color", "always"]).unwrap();
        assert!(cli.mono);
        assert_eq!(cli.color, Some(newt_core::ColorMode::Always));
        let color_kw = if cli.mono {
            Some("mono")
        } else {
            cli.color.map(|c| c.keyword())
        };
        assert_eq!(color_kw, Some("mono"));
    }

    #[test]
    fn help_suite_applies_next_line_help_to_root_and_subcommands() {
        let mut command = help_command();
        let mut root_help = Vec::new();
        command.write_long_help(&mut root_help).unwrap();
        let root_help = String::from_utf8(root_help).unwrap();

        let mut sub_help = Vec::new();
        let dgx = command.find_subcommand_mut("dgx").unwrap();
        dgx.write_long_help(&mut sub_help).unwrap();
        let dgx_help = String::from_utf8(sub_help).unwrap();

        assert!(root_help.contains("  -h, --help\n          Print help"));
        assert!(dgx_help.contains("  -h, --help\n          Print help"));
    }

    #[test]
    fn parse_color_mode_accepts_aliases_and_rejects_garbage() {
        assert_eq!(parse_color_mode("on"), Ok(newt_core::ColorMode::Always));
        assert_eq!(parse_color_mode("OFF"), Ok(newt_core::ColorMode::Never));
        assert!(parse_color_mode("chartreuse").is_err());
    }

    // ── lean / flight flag (issue #527) ─────────────────────────────────

    #[test]
    fn parses_lean_flag_and_all_aliases() {
        // -n / --neat / --lite / --lean / --flight / --plain / --no-footer all
        // set the same `plain` switch (lean morphology), bare or after `code`.
        for argv in [
            vec!["newt", "-n"],
            vec!["newt", "--neat"],
            vec!["newt", "--lite"],
            vec!["newt", "--lean"],
            vec!["newt", "--flight"],
            vec!["newt", "--plain"],
            vec!["newt", "--no-footer"],
            vec!["newt", "code", "-n"],
        ] {
            let cli = Cli::try_parse_from(argv.clone()).unwrap();
            assert!(cli.plain, "{argv:?} should set the lean/plain switch");
        }
        // Off by default.
        assert!(!Cli::try_parse_from(["newt"]).unwrap().plain);
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

    // ── plan --one-shot (#646) ──────────────────────────────────────────
    #[test]
    fn parses_plan_one_shot_flag() {
        // `--goal … --one-shot`: author AND execute in one gesture. Distinct from
        // `--execute` (the flag carries its own approval).
        let cli = Cli::try_parse_from(["newt", "plan", "--goal", "do X", "--one-shot"]).unwrap();
        match cli.command {
            Some(Command::Plan {
                one_shot,
                goal,
                execute,
                ..
            }) => {
                assert!(one_shot, "--one-shot should set one_shot");
                assert_eq!(goal.as_deref(), Some("do X"));
                assert!(!execute, "--one-shot is its own flag, not --execute");
            }
            other => panic!("expected Command::Plan, got {other:?}"),
        }
        // `--one-shot` on a FILE one-shots the existing plan end-to-end.
        let cli = Cli::try_parse_from(["newt", "plan", "plan.toml", "--one-shot"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plan { one_shot: true, .. })
        ));
        // Off by default.
        let cli = Cli::try_parse_from(["newt", "plan", "plan.toml"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plan {
                one_shot: false,
                ..
            })
        ));
    }

    #[test]
    fn parses_plan_locked_verify_flag() {
        // `--locked-verify` carries the operator's fixed gate command (the locked
        // behavioral gate). Defaults to None.
        let cli = Cli::try_parse_from([
            "newt",
            "plan",
            "--goal",
            "do X",
            "--one-shot",
            "--locked-verify",
            "cargo test --test grade_spec",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Plan {
                locked_verify,
                one_shot,
                ..
            }) => {
                assert!(one_shot);
                assert_eq!(
                    locked_verify.as_deref(),
                    Some("cargo test --test grade_spec")
                );
            }
            other => panic!("expected Command::Plan, got {other:?}"),
        }
        // Absent by default.
        let cli = Cli::try_parse_from(["newt", "plan", "--goal", "y", "--one-shot"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Plan {
                locked_verify: None,
                ..
            })
        ));
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
    fn parses_no_prompt_for_permissions_global() {
        // #721: the opt-out flag — off by default, parses bare and under `code`.
        let cli = Cli::try_parse_from(["newt"]).unwrap();
        assert!(!cli.no_prompt_for_permissions);
        let cli = Cli::try_parse_from(["newt", "--no-prompt-for-permissions"]).unwrap();
        assert!(cli.no_prompt_for_permissions);
        let cli = Cli::try_parse_from(["newt", "code", "--no-prompt-for-permissions"]).unwrap();
        assert!(cli.no_prompt_for_permissions);
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

    /// facade P4 (#780): `--no-route` parses bare and under `code`, is OFF by
    /// default, and is a DISTINCT flag from `--disable-ocap`/`--yolo` (§7-F5) —
    /// the two never alias, so the routing escape can never imply an unconfine.
    #[test]
    fn parses_no_route_distinct_from_disable_ocap() {
        let cli = Cli::try_parse_from(["newt"]).unwrap();
        assert!(!cli.no_route);
        let cli = Cli::try_parse_from(["newt", "--no-route"]).unwrap();
        assert!(cli.no_route);
        let cli = Cli::try_parse_from(["newt", "code", "--no-route"]).unwrap();
        assert!(cli.no_route);
        // --no-route does NOT set the L3-off bypass, and --yolo does NOT set
        // the routing flag: distinct mechanisms, never aliased.
        let cli = Cli::try_parse_from(["newt", "--no-route"]).unwrap();
        assert!(cli.no_route && !cli.disable_ocap);
        let cli = Cli::try_parse_from(["newt", "--yolo"]).unwrap();
        assert!(cli.disable_ocap && !cli.no_route);
    }

    /// `--full-access` parses bare and under `code`, is OFF by default, and is
    /// a DISTINCT flag from `--disable-ocap`/`--yolo`: authority-widening and
    /// the exec-mechanism bypass never alias — asking for one never implies
    /// the other.
    #[test]
    fn parses_full_access_distinct_from_disable_ocap() {
        let cli = Cli::try_parse_from(["newt"]).unwrap();
        assert!(!cli.full_access);
        let cli = Cli::try_parse_from(["newt", "--full-access"]).unwrap();
        assert!(cli.full_access && !cli.disable_ocap);
        let cli = Cli::try_parse_from(["newt", "code", "--full-access"]).unwrap();
        assert!(cli.full_access);
        assert!(matches!(cli.command, Some(Command::Code { .. })));
        let cli = Cli::try_parse_from(["newt", "--yolo"]).unwrap();
        assert!(cli.disable_ocap && !cli.full_access);
        let cli = Cli::try_parse_from(["newt", "--yolo", "--full-access"]).unwrap();
        assert!(cli.disable_ocap && cli.full_access);
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
    fn parses_setup_target_token_env_and_yes() {
        let cli = Cli::try_parse_from([
            "newt",
            "setup",
            "https://dgx1.home.lab:8000",
            "--token-env",
            "DGX_TOKEN",
            "--yes",
        ])
        .unwrap();

        match cli.command {
            Some(Command::Setup {
                target,
                token_env,
                token_file,
                yes,
            }) => {
                assert_eq!(target.as_deref(), Some("https://dgx1.home.lab:8000"));
                assert_eq!(token_env.as_deref(), Some("DGX_TOKEN"));
                assert_eq!(token_file, None);
                assert!(yes);
            }
            other => panic!("expected setup command, got {other:?}"),
        }
    }

    #[test]
    fn parses_setup_token_reference_aliases() {
        let env_cli = Cli::try_parse_from([
            "newt",
            "setup",
            "https://dgx1.home.lab:8000",
            "--api-key-env",
            "DGX_TOKEN",
        ])
        .unwrap();
        assert!(matches!(
            env_cli.command,
            Some(Command::Setup {
                token_env: Some(ref value),
                ..
            }) if value == "DGX_TOKEN"
        ));

        let file_cli = Cli::try_parse_from([
            "newt",
            "setup",
            "https://dgx1.home.lab:8080",
            "--api-key-file",
            "/tmp/dgx-token",
        ])
        .unwrap();
        assert!(matches!(
            file_cli.command,
            Some(Command::Setup {
                token_file: Some(ref value),
                ..
            }) if value == std::path::Path::new("/tmp/dgx-token")
        ));
    }

    #[test]
    fn setup_token_references_conflict() {
        let err = Cli::try_parse_from([
            "newt",
            "setup",
            "dgx1.home.lab",
            "--token-env",
            "DGX_TOKEN",
            "--token-file",
            "/tmp/dgx-token",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parses_setup_without_target_for_interactive_wizard() {
        let cli = Cli::try_parse_from(["newt", "setup"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Setup {
                target: None,
                token_env: None,
                token_file: None,
                yes: false,
            })
        ));
    }

    #[test]
    fn setup_target_only_flags_require_a_target() {
        for args in [
            vec!["newt", "setup", "--yes"],
            vec!["newt", "setup", "--token-env", "DGX_TOKEN"],
            vec!["newt", "setup", "--token-file", "/tmp/dgx-token"],
        ] {
            let err = Cli::try_parse_from(args).unwrap_err();
            assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        }
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
