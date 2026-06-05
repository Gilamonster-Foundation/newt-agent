//! Configuration loading for Newt-Agent.
//!
//! Resolution order: `$NEWT_CONFIG` env var, then `./newt.toml`,
//! `~/.newt/config.toml`, `/etc/newt/config.toml`. If none exist the
//! built-in defaults are used (a single Ollama backend on localhost).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{NewtError, Result};
use crate::router::Tier;

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// Top-level Newt-Agent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Inference backends (Ollama, vLLM, etc.).
    pub backends: Vec<BackendConfig>,

    /// External provider-plugin definitions.
    pub providers: Vec<ProviderConfig>,

    /// Default tier ordering used by the router when no per-backend
    /// override is specified.
    pub default_tier_order: Vec<Tier>,

    /// Optional NVIDIA DGX endpoint-management config powering the
    /// `newt dgx` command suite. `None` when unconfigured — newt never
    /// dials a DGX endpoint unless this (or a `NEWT_DGX_*` env var) is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dgx: Option<crate::dgx::DgxConfig>,

    /// TUI appearance and behaviour. `None` → built-in defaults apply.
    /// Overridable at runtime via `NEWT_CHAT_STYLE` and `NEWT_PROMPT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui: Option<TuiConfig>,

    /// Inference cost modeling. `None` → built-in rate table only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<crate::pricing::PricingConfig>,

    /// Memory / context-window management. `None` → RollingWindow(20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryConfig>,

    /// newt-native MCP servers (`[[mcp_servers]]`). Merged with the servers
    /// discovered from Claude Code's config by [`crate::mcp::discover`]; these
    /// take precedence on a name clash. Empty by default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<crate::mcp::McpServerEntry>,

    /// Usage-log rotation policy. `None` → built-in defaults apply
    /// (keep last 7 sessions, no size/age limit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<LogConfig>,

    /// Skill discovery search path — the ordered list of directories newt
    /// reads `SKILL.md` folders from. `None` → just `~/.newt/skills`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<SkillsConfig>,
}

// ---------------------------------------------------------------------------
// Skill search path
// ---------------------------------------------------------------------------

/// The skill discovery **search path**: an ordered list of directories newt
/// scans for agentskills.io-format `SKILL.md` folders.
///
/// A skill is the same folder in every harness, so cross-harness use is just a
/// matter of *pointing newt at the directories* — list `~/.claude/skills`,
/// `~/.codex/skills`, a project-local `.skills/`, whatever — and their skills
/// become visible with no copying. The list is open-ended on purpose: there is
/// no hard-coded knowledge of any particular harness. Earlier entries win on a
/// name collision.
///
/// Example `~/.newt/config.toml`:
/// ```toml
/// [skills]
/// search = ["~/.newt/skills", "~/.claude/skills", "~/.codex/skills"]
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Ordered directories to scan for skills. Empty → `~/.newt/skills`.
    /// `~/` is expanded to `$HOME`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search: Vec<String>,
}

// ---------------------------------------------------------------------------
// Log rotation config
// ---------------------------------------------------------------------------

/// Rotation policy for `~/.newt/usage.jsonl`.
///
/// All limits default to the values shown. Set a field to `0` to disable
/// that particular limit. Multiple active limits compose — the most
/// restrictive one wins after each append.
///
/// Example `newt.toml`:
/// ```toml
/// [logs]
/// max_sessions = 100   # keep the last 100 turns
/// max_size_mb  = 5     # also cap at 5 MiB
/// max_age_days = 14    # and drop anything older than 2 weeks
/// keep_rotated = 2     # keep usage.jsonl.1 and .2 as backup
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// Keep at most this many JSONL entries (most recent). Default: 7. 0 = no limit.
    #[serde(default = "default_log_max_sessions")]
    pub max_sessions: usize,

    /// Rotate when the file exceeds this size in MiB. Default: 0 (no size limit).
    #[serde(default)]
    pub max_size_mb: u64,

    /// Drop entries older than this many days. Default: 0 (no age limit).
    /// Requires a `recorded_at` field in the log entry; entries without it
    /// are kept.
    #[serde(default)]
    pub max_age_days: u64,

    /// How many rotated copies to keep alongside the live log
    /// (`usage.jsonl.1`, `.2`, …). Default: 3. 0 = overwrite silently.
    #[serde(default = "default_log_keep_rotated")]
    pub keep_rotated: usize,
}

fn default_log_max_sessions() -> usize {
    7
}

fn default_log_keep_rotated() -> usize {
    3
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            max_sessions: default_log_max_sessions(),
            max_size_mb: 0,
            max_age_days: 0,
            keep_rotated: default_log_keep_rotated(),
        }
    }
}

// ---------------------------------------------------------------------------
// Memory config
// ---------------------------------------------------------------------------

/// Memory management stored under `[memory]` in `newt.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Which memory provider to activate.
    #[serde(default)]
    pub provider: MemoryProviderKind,
    /// Turns retained by `RollingWindow`. Default: 20.
    #[serde(default = "default_memory_window")]
    pub window: usize,
    /// Model context length for `TokenBudget` (overrides Ollama's reported value).
    /// Default: 8192.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,

    /// Explicit path to a soul file (overrides workspace + global resolution).
    /// Default: auto-resolve from `.newt/soul.md` → `~/.newt/soul.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soul_file: Option<String>,
}

fn default_memory_window() -> usize {
    20
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            provider: MemoryProviderKind::RollingWindow,
            window: 20,
            context_tokens: None,
            soul_file: None,
        }
    }
}

/// Which built-in memory strategy to use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryProviderKind {
    #[default]
    RollingWindow,
    TokenBudget,
    Summarizing,
}

// ---------------------------------------------------------------------------
// TUI config
// ---------------------------------------------------------------------------

/// TUI appearance preferences stored under `[tui]` in `newt.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    /// Whether to show "newt" / "you" labels before the carets.
    pub chat_style: ChatStyle,

    /// PS1-style prompt template.
    ///
    /// Tokens: `\w` workspace basename, `\W` full path, `\h` hostname,
    /// `\v` newt version.  Default: `"\\w $ "` (compact) / `"you $ "` (verbose).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,

    /// Skip the full-screen ANSI art splash and show a compact header instead.
    /// Equivalent to the `--no-splash` CLI flag.
    #[serde(default)]
    pub no_splash: bool,

    /// Key binding mode for the chat input line.
    /// `"emacs"` (default) or `"vi"`. Also overridable via `NEWT_EDIT_MODE`.
    #[serde(default)]
    pub edit_mode: EditMode,

    /// Maximum lines of tool output shown inline before offering "show all?".
    /// Default: 20. Set to 0 to always show everything.
    #[serde(default = "default_tool_output_lines")]
    pub tool_output_lines: usize,

    /// Maximum number of tool-call rounds the model may take within a single
    /// turn before the agent forces a final, tools-disabled completion. Each
    /// round is one model response that may emit tool calls; once this many
    /// rounds have run without a tool-free answer, newt asks the model once
    /// more with tools disabled so the user still gets a real (partial)
    /// answer instead of a placeholder. Default: 25.
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: usize,

    /// Tool-call permission policy for the interactive TUI: which tools the
    /// model may invoke and over which targets. This is a *preset that selects
    /// an attenuation* — the host (`newt-identity`) lowers it into a signed,
    /// attenuation-only capability that enforcement consults. Default:
    /// `WorkspaceDev`.
    #[serde(default)]
    pub permissions: ToolPermissions,

    /// Enable per-round agent-loop diagnostics printed to the TUI. Shows each
    /// round's content excerpt, tool-call count, token usage, and flags empty
    /// model responses before they become silent failures. Also set via the
    /// `NEWT_DEBUG=1` environment variable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub debug: Option<bool>,

    /// Shell command to run after every successful file write or edit, to
    /// give the agent immediate ground-truth feedback on whether its change
    /// compiled / passed basic checks. Output is appended to the tool result
    /// so the model sees it without needing to ask.
    ///
    /// Set this per-workspace in `.newt/config.toml` — not globally — because
    /// the right command depends on the project's build system:
    ///
    /// ```toml
    /// [tui]
    /// build_check_cmd = "cargo check -q --workspace"  # Rust
    /// # build_check_cmd = "npm run build --silent"    # Node
    /// # build_check_cmd = "python -m py_compile"      # Python
    /// ```
    ///
    /// `None` (default) disables auto-checking — no extra command is run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_check_cmd: Option<String>,

    // -----------------------------------------------------------------------
    // DGX / inference endpoint resource management
    // -----------------------------------------------------------------------
    /// Ollama context-window cap sent as `options.num_ctx` on every request.
    /// Limits the KV-cache allocation so a large model can't exhaust VRAM
    /// mid-session. `None` → let Ollama use the model's compiled-in default
    /// (often 131k for recent models — far too large to coexist with weights
    /// on a single GPU). Recommended starting point: 8192 or 16384.
    /// Tune upward if you need longer tool-call histories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,

    /// TCP connect timeout in seconds for inference requests (default: 5).
    /// A fast failure here means the endpoint is down (connection refused),
    /// distinguishing it from a slow-but-alive endpoint that needs the full
    /// `inference_timeout_secs` to respond. Keep this short.
    #[serde(default = "default_connect_timeout_secs")]
    pub connect_timeout_secs: u64,

    /// Total inference request timeout in seconds (default: 120). This is the
    /// wall-clock budget for the model to generate a complete response —
    /// large models on a busy DGX may need the full window.
    #[serde(default = "default_inference_timeout_secs")]
    pub inference_timeout_secs: u64,

    /// How long Ollama keeps a model resident in VRAM after the last request,
    /// as an Ollama duration string (e.g. `"5m"`, `"0"`, `"-1"`).
    /// Default: `"5m"`. Use `"0"` to unload immediately after each turn
    /// (maximum headroom for multi-model or multi-agent workloads at the cost
    /// of a reload on each turn). Use `"-1"` to keep forever.
    #[serde(default = "default_keep_alive")]
    pub keep_alive: String,

    /// Maximum number of messages in the in-progress tool-call message list
    /// before the agent trims the middle to prevent context overflow.
    /// Default: 40 (≈ 20 tool-call rounds). Set lower on memory-constrained
    /// endpoints or when `num_ctx` is small.
    #[serde(default = "default_mid_loop_trim_threshold")]
    pub mid_loop_trim_threshold: usize,
}

fn default_tool_output_lines() -> usize {
    20
}

fn default_max_tool_rounds() -> usize {
    25
}

fn default_connect_timeout_secs() -> u64 {
    5
}

fn default_inference_timeout_secs() -> u64 {
    120
}

fn default_keep_alive() -> String {
    "5m".to_string()
}

fn default_mid_loop_trim_threshold() -> usize {
    40
}

// ---------------------------------------------------------------------------
// Tool permissions — preset policies, lowered to attenuated capabilities
// ---------------------------------------------------------------------------

/// A named tool-permission preset for the TUI tool loop.
///
/// Each preset selects a [`crate::Caveats`] *policy* via
/// [`ToolPermissions::to_caveats`]; the host (`newt-identity`) then lowers that
/// policy into a signed, attenuation-only capability for enforcement. A preset
/// is a name-based convenience, **not** a capability itself — the unforgeable
/// authority is the signed `AgentKey` delegation. `Custom` means the user has
/// added commands beyond a canned preset; it carries `WorkspaceDev` authority
/// plus those extras (it does **not** grant full access).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPreset {
    /// Read files and list dirs only; no writes, no commands.
    ReadOnly,
    /// Read + write within the workspace; no shell commands.
    WorkspaceEdit,
    /// Read, write workspace, run a conservative set of dev tools.
    /// See [`ToolPermissions::to_caveats`] for the exact allowlist.
    #[default]
    WorkspaceDev,
    /// Unrestricted — `Caveats::top()`. `write_file` still prompts y/N.
    FullAccess,
    /// User has added commands beyond a canned preset; carries `WorkspaceDev`
    /// authority plus those `extra_exec` entries — **not** full access.
    Custom,
}

impl PermissionPreset {
    pub const ALL: [Self; 4] = [
        Self::ReadOnly,
        Self::WorkspaceEdit,
        Self::WorkspaceDev,
        Self::FullAccess,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceEdit => "workspace_edit",
            Self::WorkspaceDev => "workspace_dev",
            Self::FullAccess => "full_access",
            Self::Custom => "custom",
        }
    }

    /// Cycle through the four user-visible presets (skips `Custom`).
    pub fn toggle(&self) -> Self {
        let idx = Self::ALL.iter().position(|p| p == self).unwrap_or(2);
        Self::ALL[(idx + 1) % Self::ALL.len()].clone()
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read files + list dirs; no writes, no commands",
            Self::WorkspaceEdit => "read + write workspace; no shell commands",
            Self::WorkspaceDev => "read, write workspace, run: cargo just git grep rg fd ...",
            Self::FullAccess => "unrestricted (prompts y/N before each write)",
            Self::Custom => "workspace-dev tools plus your extra commands",
        }
    }
}

/// Permission configuration stored under `[tui.permissions]` in `newt.toml`.
///
/// Call [`ToolPermissions::to_caveats`] to obtain the runtime [`crate::Caveats`]
/// enforced by every `execute_tool` dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolPermissions {
    /// The active preset.
    pub preset: PermissionPreset,

    /// Extra commands allowed beyond the `WorkspaceDev` built-in set.
    /// Only consulted when `preset == WorkspaceDev` or `Custom`.
    /// Stored as leading tokens, e.g. `["bacon", "make"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_exec: Vec<String>,

    /// Hosts the agent may reach with `web_fetch` (the `net` capability axis).
    ///
    /// Empty (the default) = **no network** — `web_fetch` is denied. A single
    /// `"*"` grants **all** hosts (still SSRF-screened + DNS-rebind-pinned by the
    /// web tool). Otherwise an exact host allowlist, e.g.
    /// `["docs.rs", "raw.githubusercontent.com"]`. Applies to every preset
    /// except `FullAccess` (which is already unrestricted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub net: Vec<String>,
}

impl Default for ToolPermissions {
    fn default() -> Self {
        Self {
            preset: PermissionPreset::WorkspaceDev,
            extra_exec: Vec::new(),
            net: Vec::new(),
        }
    }
}

impl ToolPermissions {
    /// Built-in exec allowlist for the `WorkspaceDev` preset.
    const WORKSPACE_DEV_EXEC: &'static [&'static str] = &[
        "cargo",
        // rustc must be here: cargo spawns it as a subprocess to compile and
        // test. Without it, `cargo test` fails with "could not execute rustc".
        // rustfmt and clippy-driver are already present; this was an oversight.
        "rustc",
        "just",
        "git",
        "grep",
        "rg",
        "ripgrep",
        "fd",
        "find",
        "cat",
        "ls",
        "echo",
        "pwd",
        "true",
        "false",
        "head",
        "tail",
        "wc",
        "sort",
        "uniq",
        "diff",
        "patch",
        "rustfmt",
        "clippy-driver",
        "rustup",
        // Polyglot dev tools reached for routinely in a mixed workspace. Same
        // risk tier as cargo/git — WorkspaceDev already grants workspace write
        // and the full Rust toolchain. Anything outside this set can still be
        // opted in per-config via `[tui.permissions] extra_exec = [...]`.
        "gh",
        "python",
        "python3",
        "pip",
        "npm",
        "node",
        "make",
        "jq",
        "curl",
        "awk",
        "sed",
        "cut",
        "xargs",
        "which",
        "env",
    ];

    /// Build the runtime `Caveats` for this permission configuration.
    ///
    /// `workspace` is the absolute path to the current workspace directory;
    /// it is stored in `Scope::Only` so the TUI enforcement layer can do
    /// prefix matching (path within workspace → permitted).
    ///
    /// Note: the `Caveats` lattice uses exact-set semantics; prefix matching
    /// is the responsibility of the enforcement site (`tui_permits_path` in
    /// newt-tui), not this algebra. This is an intentional layer separation.
    pub fn to_caveats(&self, workspace: &str) -> crate::caveats::Caveats {
        use crate::caveats::{Caveats, CountBound, Scope};

        let ws = workspace.to_string();
        let net = self.net_scope();

        match self.preset {
            PermissionPreset::ReadOnly => Caveats {
                fs_read: Scope::All,
                fs_write: Scope::none(),
                exec: Scope::none(),
                net,
                max_calls: CountBound::Unlimited,
                valid_for_generation: Scope::All,
            },

            PermissionPreset::WorkspaceEdit => Caveats {
                fs_read: Scope::All,
                fs_write: Scope::only([ws]),
                exec: Scope::none(),
                net,
                max_calls: CountBound::Unlimited,
                valid_for_generation: Scope::All,
            },

            // `Custom` shares this arm: editing `extra_exec` keeps WorkspaceDev
            // authority plus the added commands. It must NOT escalate to
            // `top()` — adding one command to an allowlist should never grant
            // full access.
            PermissionPreset::WorkspaceDev | PermissionPreset::Custom => {
                let mut allowed: std::collections::BTreeSet<String> = Self::WORKSPACE_DEV_EXEC
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                for cmd in &self.extra_exec {
                    allowed.insert(cmd.clone());
                }
                Caveats {
                    fs_read: Scope::All,
                    fs_write: Scope::only([ws]),
                    exec: Scope::Only(allowed),
                    net,
                    max_calls: CountBound::Unlimited,
                    valid_for_generation: Scope::All,
                }
            }

            PermissionPreset::FullAccess => Caveats::top(),
        }
    }

    /// Lower the configured `net` allowlist into a capability [`Scope`].
    ///
    /// Empty → `none` (no network). A `"*"` entry → `All` (every host, still
    /// SSRF-screened by the web tool). Otherwise an exact host allowlist.
    fn net_scope(&self) -> crate::caveats::Scope<String> {
        use crate::caveats::Scope;
        if self.net.is_empty() {
            Scope::none()
        } else if self.net.iter().any(|h| h == "*") {
            Scope::All
        } else {
            Scope::only(self.net.iter().cloned())
        }
    }
}

/// Key binding style for the chat REPL input line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    /// Readline / emacs-style bindings (default).
    #[default]
    Emacs,
    /// Vi / vim-style bindings — Esc for normal mode, i for insert.
    Vi,
}

impl EditMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Emacs => "emacs",
            Self::Vi => "vi",
        }
    }

    pub fn toggle(&self) -> Self {
        match self {
            Self::Emacs => Self::Vi,
            Self::Vi => Self::Emacs,
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            chat_style: ChatStyle::Compact,
            prompt: None,
            no_splash: false,
            edit_mode: EditMode::Emacs,
            tool_output_lines: default_tool_output_lines(),
            max_tool_rounds: default_max_tool_rounds(),
            permissions: ToolPermissions::default(),
            debug: None,
            build_check_cmd: None,
            num_ctx: None,
            connect_timeout_secs: default_connect_timeout_secs(),
            inference_timeout_secs: default_inference_timeout_secs(),
            keep_alive: default_keep_alive(),
            mid_loop_trim_threshold: default_mid_loop_trim_threshold(),
        }
    }
}

/// Chat REPL display density.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ChatStyle {
    /// Just the caret symbol — no "newt" / "you" labels.
    #[default]
    Compact,
    /// Full "newt ▸" / "you $" labels before each message.
    Verbose,
}

impl ChatStyle {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Verbose => "verbose",
        }
    }

    pub fn toggle(&self) -> Self {
        match self {
            Self::Compact => Self::Verbose,
            Self::Verbose => Self::Compact,
        }
    }
}

/// The wire protocol an inference backend speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Ollama's native `POST /api/chat` API (the historical default).
    #[default]
    Ollama,
    /// An OpenAI-compatible HTTP API (`POST /v1/chat/completions`,
    /// `GET /v1/models`): vLLM, llama.cpp's server, or any hosted
    /// OpenAI-compatible endpoint. Optionally authenticated with a
    /// bearer token (see [`BackendConfig::api_key_file`] /
    /// [`BackendConfig::api_key_env`]).
    #[serde(alias = "vllm", alias = "openai-compatible")]
    Openai,
}

/// A single inference backend entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    pub name: String,
    pub endpoint: String,
    pub model: String,
    pub tiers: Vec<Tier>,
    /// Which wire protocol this backend speaks. Defaults to `ollama`
    /// so configs written before this field existed keep working.
    #[serde(default)]
    pub kind: BackendKind,
    /// Optional path to a file whose first non-empty line is a bearer
    /// token, sent as `Authorization: Bearer <token>` by
    /// OpenAI-compatible backends. A leading `~/` is expanded to the
    /// home directory. Keeping the secret in a file (rather than inline
    /// in the config) keeps tokens out of version control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<String>,
    /// Optional environment variable name holding a bearer token. Takes
    /// precedence over [`api_key_file`](Self::api_key_file) when both
    /// resolve to a non-empty value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

impl BackendConfig {
    /// Resolve this backend's bearer token, if any.
    ///
    /// Checks [`api_key_env`](Self::api_key_env) first (environment
    /// variable), then [`api_key_file`](Self::api_key_file) (first
    /// non-empty line of the file, trimmed). Returns `None` when neither
    /// is configured or neither resolves to a non-empty value.
    pub fn resolve_api_key(&self) -> Option<String> {
        if let Some(var) = &self.api_key_env {
            if let Ok(val) = std::env::var(var) {
                let val = val.trim();
                if !val.is_empty() {
                    return Some(val.to_string());
                }
            }
        }
        if let Some(path) = &self.api_key_file {
            let expanded = expand_tilde(path);
            if let Ok(contents) = std::fs::read_to_string(&expanded) {
                if let Some(token) = contents.lines().map(str::trim).find(|l| !l.is_empty()) {
                    return Some(token.to_string());
                }
            }
        }
        None
    }
}

/// A subprocess provider-plugin entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub env_pass: Vec<String>,
    pub tiers: Vec<Tier>,
}

// ---------------------------------------------------------------------------
// Default
// ---------------------------------------------------------------------------

impl Default for Config {
    fn default() -> Self {
        Self {
            backends: vec![BackendConfig {
                name: "ollama".into(),
                endpoint: "http://127.0.0.1:11434".into(),
                model: "llama3.1:8b".into(),
                tiers: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
                kind: BackendKind::Ollama,
                api_key_file: None,
                api_key_env: None,
            }],
            providers: Vec::new(),
            default_tier_order: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
            dgx: None,
            tui: None,
            pricing: None,
            memory: None,
            mcp_servers: Vec::new(),
            logs: None,
            skills: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl Config {
    /// Load configuration from an explicit file path.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| NewtError::Config(e.to_string()))
    }

    /// Resolve configuration by searching well-known locations.
    ///
    /// Search order:
    /// 1. `$NEWT_CONFIG` environment variable
    /// 2. `./newt.toml`
    /// 3. `~/.newt/config.toml`
    /// 4. `/etc/newt/config.toml`
    ///
    /// Returns `Config::default()` if none of the candidates exist.
    pub fn resolve() -> Result<Self> {
        let candidates = Self::candidate_paths();
        for path in &candidates {
            if path.is_file() {
                return Self::load(path);
            }
        }
        Ok(Self::default())
    }

    /// The user-writable config path: `~/.newt/config.toml`.
    /// This is the first path `resolve()` reads and the target for `save()`.
    pub fn user_config_path() -> Option<PathBuf> {
        home_dir().map(|h| h.join(".newt").join("config.toml"))
    }

    /// The ordered skill-discovery search path, with `~/` expanded.
    ///
    /// Resolves `[skills].search` when configured; otherwise defaults to the
    /// single host-scoped `~/.newt/skills`. Order is preserved — earlier
    /// directories win on a name collision (see `newt_skills::discover_paths`).
    /// The default falls back to a relative `.newt/skills` only when `$HOME`
    /// can't be resolved, so the list is never empty.
    #[must_use]
    pub fn skill_search_dirs(&self) -> Vec<PathBuf> {
        let configured = self
            .skills
            .as_ref()
            .map(|s| s.search.as_slice())
            .unwrap_or(&[]);
        if configured.is_empty() {
            let default = home_dir()
                .map(|h| h.join(".newt").join("skills"))
                .unwrap_or_else(|| PathBuf::from(".newt/skills"));
            return vec![default];
        }
        configured.iter().map(|s| expand_tilde(s)).collect()
    }

    /// Serialize this config and write it to `path`, creating parent dirs if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(NewtError::Io)?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| NewtError::Config(e.to_string()))?;
        std::fs::write(path, text).map_err(NewtError::Io)
    }

    /// Build the ordered list of candidate config file paths.
    fn candidate_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        if let Ok(p) = std::env::var("NEWT_CONFIG") {
            paths.push(PathBuf::from(p));
        }

        paths.push(PathBuf::from("./newt.toml"));

        if let Some(home) = home_dir() {
            paths.push(home.join(".newt").join("config.toml"));
        }

        paths.push(PathBuf::from("/etc/newt/config.toml"));
        paths
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Best-effort home directory lookup without pulling in the `dirs` crate.
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

/// Expand a leading `~/` (or a bare `~`) to the home directory. Paths
/// without a leading tilde are returned unchanged.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    } else if path == "~" {
        if let Some(home) = home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    // The `permits_*` adaptors live on `CaveatsExt` (post-#95 the
    // upstream `agent-mesh-protocol::Caveats` ships algebra only).
    use crate::caveats::CaveatsExt;
    use std::io::Write;

    #[test]
    fn skill_search_dirs_defaults_to_single_newt_dir() {
        let cfg = Config::default();
        let dirs = cfg.skill_search_dirs();
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].ends_with("skills"));
        // The parent component is `.newt`.
        assert_eq!(
            dirs[0].parent().and_then(|p| p.file_name()),
            Some(".newt".as_ref())
        );
    }

    #[test]
    fn skill_search_dirs_preserves_configured_order() {
        let cfg = Config {
            skills: Some(SkillsConfig {
                search: vec!["/abs/one".into(), "/abs/two".into()],
            }),
            ..Config::default()
        };
        assert_eq!(
            cfg.skill_search_dirs(),
            vec![PathBuf::from("/abs/one"), PathBuf::from("/abs/two")]
        );
    }

    #[test]
    fn skill_search_dirs_expands_tilde() {
        let cfg = Config {
            skills: Some(SkillsConfig {
                search: vec!["~/skills-x".into()],
            }),
            ..Config::default()
        };
        let dirs = cfg.skill_search_dirs();
        // The final component survives expansion regardless of whether $HOME
        // was set; when set, the leading `~` must be gone.
        assert!(dirs[0].ends_with("skills-x"));
        assert!(!dirs[0].starts_with("~"));
    }

    #[test]
    fn skills_search_round_trips_through_toml() {
        let cfg = Config {
            skills: Some(SkillsConfig {
                search: vec!["~/.newt/skills".into(), "~/.claude/skills".into()],
            }),
            ..Config::default()
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            back.skills.unwrap().search,
            vec!["~/.newt/skills".to_string(), "~/.claude/skills".to_string()]
        );
    }
    use tempfile::NamedTempFile;

    #[test]
    fn defaults_are_sensible() {
        let cfg = Config::default();
        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(cfg.providers.len(), 0);
        assert_eq!(cfg.default_tier_order.len(), 4);
    }

    #[test]
    fn load_happy_path() {
        let toml_text = r#"
[[backends]]
name = "local-ollama"
endpoint = "http://localhost:11434"
model = "mistral:7b"
tiers = ["FAST", "STANDARD"]

[[providers]]
name = "cloud"
command = "newt-cloud-shim"
env_pass = ["CLOUD_TOKEN"]
tiers = ["COMPLEX", "REVIEW"]

default_tier_order = ["FAST", "STANDARD", "COMPLEX", "REVIEW"]
"#;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(toml_text.as_bytes()).unwrap();
        f.flush().unwrap();

        let cfg = Config::load(f.path()).unwrap();
        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(cfg.backends[0].name, "local-ollama");
        assert_eq!(cfg.backends[0].model, "mistral:7b");
        assert_eq!(cfg.backends[0].tiers, vec![Tier::Fast, Tier::Standard]);
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].name, "cloud");
        assert_eq!(cfg.providers[0].env_pass, vec!["CLOUD_TOKEN".to_string()]);
    }

    #[test]
    fn missing_file_returns_io_error() {
        let result = Config::load(Path::new("/tmp/newt-does-not-exist-12345.toml"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, NewtError::Io(_)),
            "expected Io error, got: {err:?}"
        );
    }

    #[test]
    fn malformed_toml_returns_config_error() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"{{{{").unwrap();
        f.flush().unwrap();

        let result = Config::load(f.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, NewtError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[test]
    fn resolve_returns_default_when_no_file() {
        // Use a temp dir as cwd and clear env to ensure no candidates match.
        let dir = tempfile::tempdir().unwrap();

        // Save & clear environment to isolate the test.
        let saved_config = std::env::var("NEWT_CONFIG").ok();
        let saved_home = std::env::var("HOME").ok();
        std::env::remove_var("NEWT_CONFIG");
        std::env::set_var("HOME", dir.path());

        // Run resolve from inside the temp dir so ./newt.toml won't exist.
        let prev_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let cfg = Config::resolve().unwrap();

        // Restore environment.
        std::env::set_current_dir(prev_dir).unwrap();
        if let Some(v) = saved_home {
            std::env::set_var("HOME", v);
        }
        if let Some(v) = saved_config {
            std::env::set_var("NEWT_CONFIG", v);
        }

        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(cfg.backends[0].name, "ollama");
    }

    #[test]
    fn config_default_has_no_dgx() {
        assert!(Config::default().dgx.is_none());
    }

    #[test]
    fn config_with_dgx_roundtrips() {
        let cfg = Config {
            dgx: Some(crate::dgx::DgxConfig::home_template()),
            ..Config::default()
        };
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back = toml::from_str::<Config>(&text).unwrap();
        let dgx = back.dgx.expect("dgx should round-trip");
        assert_eq!(dgx.active_node.as_deref(), Some("home"));
        assert_eq!(dgx.nodes.len(), 1);
        assert_eq!(dgx.formations.len(), 2);
    }

    // --- ToolPermissions / to_caveats ---

    #[test]
    fn workspace_dev_allows_cargo_and_just() {
        let perms = ToolPermissions::default(); // WorkspaceDev
        let cav = perms.to_caveats("/workspace");
        assert!(cav.permits_exec("cargo"), "cargo must be allowed");
        assert!(cav.permits_exec("just"), "just must be allowed");
        assert!(cav.permits_exec("git"), "git must be allowed");
    }

    #[test]
    fn workspace_dev_blocks_rm_and_mv() {
        let perms = ToolPermissions::default();
        let cav = perms.to_caveats("/workspace");
        assert!(!cav.permits_exec("rm"), "rm must be blocked");
        assert!(!cav.permits_exec("mv"), "mv must be blocked");
        assert!(!cav.permits_exec("sudo"), "sudo must be blocked");
    }

    #[test]
    fn workspace_dev_allows_common_dev_tools() {
        // Regression: these were denied under the default preset even though
        // they're the same risk tier as cargo/git (issue #149). `gh` in
        // particular is authenticated outside but was blocked in-agent.
        let cav = ToolPermissions::default().to_caveats("/workspace");
        for tool in [
            "gh", "python", "python3", "pip", "npm", "node", "make", "jq", "curl", "awk", "sed",
            "cut", "xargs", "which", "env",
        ] {
            assert!(cav.permits_exec(tool), "`{tool}` must be allowed");
        }
        // Adding tools must NOT escalate to full access — destructive commands
        // outside the allowlist stay blocked.
        assert!(!cav.permits_exec("rm"), "rm must still be blocked");
        assert!(!cav.permits_exec("sudo"), "sudo must still be blocked");
    }

    #[test]
    fn workspace_dev_allows_extra_exec() {
        let perms = ToolPermissions {
            preset: PermissionPreset::WorkspaceDev,
            extra_exec: vec!["bacon".into(), "make".into()],
            net: vec![],
        };
        let cav = perms.to_caveats("/workspace");
        assert!(cav.permits_exec("bacon"));
        assert!(cav.permits_exec("make"));
        assert!(!cav.permits_exec("rm")); // extra_exec does not weaken the block
    }

    #[test]
    fn read_only_blocks_writes_and_exec() {
        let perms = ToolPermissions {
            preset: PermissionPreset::ReadOnly,
            extra_exec: vec![],
            net: vec![],
        };
        let cav = perms.to_caveats("/workspace");
        assert!(!cav.permits_fs_write("/workspace/src/main.rs"));
        assert!(!cav.permits_exec("cargo"));
        assert!(cav.permits_fs_read("/workspace/src/main.rs"));
    }

    #[test]
    fn workspace_edit_allows_write_blocks_exec() {
        let perms = ToolPermissions {
            preset: PermissionPreset::WorkspaceEdit,
            extra_exec: vec![],
            net: vec![],
        };
        let cav = perms.to_caveats("/workspace");
        assert!(!cav.permits_exec("cargo"));
        // The caveat stores workspace root; prefix matching is in the TUI layer.
        // Here we just verify the lattice is set up correctly (not All, not none).
        use crate::caveats::Scope;
        assert!(matches!(cav.fs_write, Scope::Only(_)));
    }

    #[test]
    fn full_access_is_top() {
        let perms = ToolPermissions {
            preset: PermissionPreset::FullAccess,
            extra_exec: vec![],
            net: vec![],
        };
        let cav = perms.to_caveats("/workspace");
        assert_eq!(cav, crate::caveats::Caveats::top());
    }

    #[test]
    fn net_allowlist_controls_the_net_axis() {
        use crate::caveats::Scope;

        // Default (empty `net`) => no network: web_fetch is denied.
        let none = ToolPermissions::default().to_caveats("/ws");
        assert!(
            matches!(none.net, Scope::Only(ref s) if s.is_empty()),
            "empty net config must yield an empty (deny-all) net scope"
        );

        // Explicit host allowlist — works under ANY preset (here ReadOnly), so
        // web access does not require granting writes/exec.
        let hosts = ToolPermissions {
            preset: PermissionPreset::ReadOnly,
            extra_exec: vec![],
            net: vec!["docs.rs".into(), "github.com".into()],
        }
        .to_caveats("/ws");
        assert!(
            matches!(hosts.net, Scope::Only(ref s) if s.contains("docs.rs") && s.contains("github.com")),
            "explicit hosts must populate the net allowlist"
        );

        // A single "*" grants all hosts (still SSRF-screened by the web tool).
        let all = ToolPermissions {
            preset: PermissionPreset::WorkspaceDev,
            extra_exec: vec![],
            net: vec!["*".into()],
        }
        .to_caveats("/ws");
        assert!(
            matches!(all.net, Scope::All),
            "a `*` entry must grant the whole net axis"
        );
    }

    #[test]
    fn custom_is_workspace_dev_not_top() {
        // Regression: editing the exec allowlist auto-flips the preset to
        // `Custom`, which used to map to `Caveats::top()` — a silent escalation
        // from "add one command" to "full access". `Custom` must now carry
        // WorkspaceDev authority plus the extra commands, never `top()`.
        let custom = ToolPermissions {
            preset: PermissionPreset::Custom,
            extra_exec: vec!["bacon".into()],
            net: vec![],
        }
        .to_caveats("/workspace");
        assert_ne!(
            custom,
            crate::caveats::Caveats::top(),
            "Custom must not be full access"
        );
        assert!(custom.permits_exec("cargo"), "workspace-dev tools allowed");
        assert!(custom.permits_exec("bacon"), "extra_exec command allowed");
        assert!(!custom.permits_exec("rm"), "non-allowlisted command denied");
        // Identical to WorkspaceDev with the same extras.
        let workspace_dev = ToolPermissions {
            preset: PermissionPreset::WorkspaceDev,
            extra_exec: vec!["bacon".into()],
            net: vec![],
        }
        .to_caveats("/workspace");
        assert_eq!(
            custom, workspace_dev,
            "Custom carries WorkspaceDev authority + extras"
        );
    }

    #[test]
    fn preset_toggle_cycles() {
        assert_eq!(
            PermissionPreset::ReadOnly.toggle(),
            PermissionPreset::WorkspaceEdit
        );
        assert_eq!(
            PermissionPreset::WorkspaceEdit.toggle(),
            PermissionPreset::WorkspaceDev
        );
        assert_eq!(
            PermissionPreset::WorkspaceDev.toggle(),
            PermissionPreset::FullAccess
        );
        assert_eq!(
            PermissionPreset::FullAccess.toggle(),
            PermissionPreset::ReadOnly
        );
    }

    #[test]
    fn tool_permissions_toml_roundtrip() {
        let perms = ToolPermissions {
            preset: PermissionPreset::WorkspaceDev,
            extra_exec: vec!["bacon".into()],
            net: vec![],
        };
        let toml = toml::to_string(&perms).unwrap();
        assert!(toml.contains("workspace_dev"));
        assert!(toml.contains("bacon"));
        let back: ToolPermissions = toml::from_str(&toml).unwrap();
        assert_eq!(back, perms);
    }

    fn openai_backend(api_key_file: Option<String>, api_key_env: Option<String>) -> BackendConfig {
        BackendConfig {
            name: "remote".into(),
            endpoint: "https://example.test".into(),
            model: "some-model".into(),
            tiers: vec![Tier::Fast],
            kind: BackendKind::Openai,
            api_key_file,
            api_key_env,
        }
    }

    #[test]
    fn backend_kind_defaults_to_ollama_when_absent() {
        let toml = r#"
            [[backends]]
            name = "local"
            endpoint = "http://localhost:8000"
            model = "m"
            tiers = ["FAST"]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.backends[0].kind, BackendKind::Ollama);
        assert!(cfg.backends[0].api_key_file.is_none());
        assert!(cfg.backends[0].api_key_env.is_none());
    }

    #[test]
    fn backend_kind_parses_openai_and_aliases() {
        for kind_str in ["openai", "vllm", "openai-compatible"] {
            let toml = format!(
                "[[backends]]\nname=\"x\"\nendpoint=\"http://e\"\nmodel=\"m\"\ntiers=[\"FAST\"]\nkind=\"{kind_str}\"\n"
            );
            let cfg: Config = toml::from_str(&toml).unwrap();
            assert_eq!(cfg.backends[0].kind, BackendKind::Openai, "kind={kind_str}");
        }
    }

    #[test]
    fn backend_config_roundtrips_auth_fields() {
        let cfg = openai_backend(Some("~/.newt/token".into()), Some("MY_TOKEN".into()));
        let toml = toml::to_string(&cfg).unwrap();
        assert!(toml.contains("kind = \"openai\""));
        assert!(toml.contains("api_key_file"));
        assert!(toml.contains("api_key_env"));
        let back: BackendConfig = toml::from_str(&toml).unwrap();
        assert_eq!(back.kind, BackendKind::Openai);
        assert_eq!(back.api_key_file.as_deref(), Some("~/.newt/token"));
        assert_eq!(back.api_key_env.as_deref(), Some("MY_TOKEN"));
    }

    #[test]
    fn resolve_api_key_reads_first_nonempty_line_of_file() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // Leading blank line + surrounding whitespace must be skipped/trimmed.
        write!(f, "\n  secret-token-123  \nignored-second-line\n").unwrap();
        let cfg = openai_backend(Some(f.path().to_string_lossy().into_owned()), None);
        assert_eq!(cfg.resolve_api_key().as_deref(), Some("secret-token-123"));
    }

    #[test]
    fn resolve_api_key_env_takes_precedence_over_file() {
        let var = "NEWT_TEST_API_KEY_PRECEDENCE";
        std::env::set_var(var, "  from-env  ");
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "from-file").unwrap();
        let cfg = openai_backend(
            Some(f.path().to_string_lossy().into_owned()),
            Some(var.into()),
        );
        assert_eq!(cfg.resolve_api_key().as_deref(), Some("from-env"));
        std::env::remove_var(var);
    }

    #[test]
    fn resolve_api_key_none_when_unconfigured() {
        assert_eq!(openai_backend(None, None).resolve_api_key(), None);
    }

    #[test]
    fn resolve_api_key_none_for_missing_file() {
        let cfg = openai_backend(Some("/no/such/newt/token/file".into()), None);
        assert_eq!(cfg.resolve_api_key(), None);
    }

    #[test]
    fn expand_tilde_expands_home_and_passes_through() {
        let home = home_dir().expect("HOME set in test env");
        assert_eq!(expand_tilde("~/foo/bar"), home.join("foo/bar"));
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(
            expand_tilde("relative/path"),
            PathBuf::from("relative/path")
        );
    }

    #[test]
    fn default_max_tool_rounds_is_25() {
        // The function default and the struct default agree on 25.
        assert_eq!(default_max_tool_rounds(), 25);
        assert_eq!(TuiConfig::default().max_tool_rounds, 25);
    }

    #[test]
    fn tui_max_tool_rounds_defaults_when_field_absent() {
        // An empty `[tui]` table => serde default kicks in => 25.
        let toml = r#"
            [tui]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.tui.unwrap().max_tool_rounds, 25);
    }

    #[test]
    fn tui_max_tool_rounds_can_be_overridden() {
        let toml = r#"
            [tui]
            max_tool_rounds = 7
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.tui.unwrap().max_tool_rounds, 7);
    }
}
