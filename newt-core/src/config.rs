//! Configuration loading for Newt-Agent.
//!
//! Base resolution order: `$NEWT_CONFIG` env var, then `./newt.toml`,
//! `~/.newt/config.toml`, `/etc/newt/config.toml`. If none exist the
//! built-in defaults are used (a single Ollama backend on localhost).
//!
//! A project-local `.newt/config.toml` (found by walking up from the current
//! directory) is then deep-merged **over** that base, so a git repo can pin its
//! own models, endpoints, rules, and local stdio MCP services without copying
//! the whole global config. See [`Config::resolve`] and issue #222.

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

    /// Project-instruction loading (`AGENTS.md` / `CLAUDE.md`) into the system
    /// prompt. Enabled by default. Overridable via `--agents-file` /
    /// `--no-agents-file`.
    #[serde(default)]
    pub agents: AgentsConfig,

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

    /// Per-model inference tuning overrides (`[[model_tuning]]`).
    ///
    /// Each entry locks specific parameters for a named model. Values here
    /// take precedence over empirically derived values from
    /// `model-capabilities.json` and over global `[tui]` defaults.
    ///
    /// Example `~/.newt/config.toml`:
    /// ```toml
    /// [[model_tuning]]
    /// model = "nemotron3:33b"
    /// num_ctx = 24576            # explicit Ollama context window
    /// mid_loop_trim_threshold = 12
    /// max_tool_rounds = 20
    /// ```
    ///
    /// Human-authored entries are never overwritten by the auto-tuner.
    /// Auto-tuned entries are **appended** by the harness when
    /// `tune_confidence` reaches `High`; delete or edit them freely.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_tuning: Vec<ModelTuning>,

    /// Durable conversation save/restore policy. `None` uses built-in defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversations: Option<ConversationsConfig>,

    /// How a project-local `.newt/config.toml` is layered over the global
    /// config (issue #222). `None` → built-in default (arrays replace).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeConfig>,
}

// ---------------------------------------------------------------------------
// Project-local config layering (issue #222)
// ---------------------------------------------------------------------------

/// How arrays (`[[backends]]`, `[[providers]]`, `[[mcp_servers]]`,
/// `[[model_tuning]]`) are combined when a project-local `.newt/config.toml`
/// is layered over the global config.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArrayMergeStrategy {
    /// The project array replaces the global array wholesale. Predictable and
    /// safe — the project fully owns that list. **Default.**
    #[default]
    Replace,
    /// The project array is appended to the global array (global entries first,
    /// then the project's). Additive — e.g. register an extra local stdio MCP
    /// server without redefining the global ones.
    Append,
}

/// Controls how a project-local `.newt/config.toml` is merged over the global
/// config. Tables always merge recursively (project keys win); this only
/// governs array handling. See issue #222.
///
/// Example project `.newt/config.toml`:
/// ```toml
/// [merge]
/// arrays = "append"     # add to the global lists instead of replacing them
///
/// [[mcp_servers]]
/// name = "project-fs"
/// command = "mcp-fs"
/// args = ["--root", "."]
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MergeConfig {
    /// Array-combination strategy. Default: [`ArrayMergeStrategy::Replace`].
    #[serde(default)]
    pub arrays: ArrayMergeStrategy,
}

// ---------------------------------------------------------------------------
// Durable conversation config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConversationsConfig {
    /// Maximum saved conversations per workspace. Default: 100. 0 = no pruning.
    #[serde(default = "default_conversations_max_per_workspace")]
    pub max_per_workspace: usize,
}

fn default_conversations_max_per_workspace() -> usize {
    100
}

impl Default for ConversationsConfig {
    fn default() -> Self {
        Self {
            max_per_workspace: default_conversations_max_per_workspace(),
        }
    }
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
    /// Explicit context-token budget for `TokenBudget` / `Summarizing` — a
    /// deliberate user override that wins over everything else (Step 18.2,
    /// #247). When unset, the budget derives from the empirical capability
    /// cache (`max_ok_input` else `safe_context` in
    /// `model-capabilities.json`); the static default
    /// (`DEFAULT_CONTEXT_TOKENS`, 8,192) applies only when neither exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,

    /// Explicit path to a soul file (overrides workspace + global resolution).
    /// Default: auto-resolve from `.newt/soul.md` → `~/.newt/soul.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soul_file: Option<String>,

    /// User turns without an organic `save_note` call before the in-band
    /// memory nudge is appended to the next user message (Step 19.3, #248).
    /// `0` disables the nudge. Default: 10.
    #[serde(default = "default_note_nudge_interval")]
    pub note_nudge_interval: usize,
}

fn default_memory_window() -> usize {
    20
}

fn default_note_nudge_interval() -> usize {
    10
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            provider: MemoryProviderKind::RollingWindow,
            window: 20,
            context_tokens: None,
            soul_file: None,
            note_nudge_interval: 10,
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
// Project-instruction (AGENTS.md / CLAUDE.md) config
// ---------------------------------------------------------------------------

/// Project-instruction loading stored under `[agents]` in `newt.toml`.
///
/// When enabled (the default), newt reads `AGENTS.md` / `CLAUDE.md` from the
/// workspace and injects them into the agent's system prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsConfig {
    /// Whether to load project instructions into the system prompt. Default: true.
    pub enabled: bool,
    /// Directory to search for `AGENTS.md` / `CLAUDE.md`, or a specific
    /// instructions file. Relative paths are resolved against the workspace.
    /// Default: the workspace root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
        }
    }
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

    /// Estimated-token threshold that triggers a mid-loop context trim,
    /// independent of `mid_loop_trim_threshold` (which counts *messages*).
    /// A single tool round can return a multi-KB file listing or JSON payload
    /// that adds hundreds of thousands of tokens in one message — far below the
    /// message-count threshold but well past the model's context window. When
    /// set, trimming fires as soon as the estimated token count (chars / 4)
    /// exceeds this value. `None` disables token-based trimming.
    /// Default: `None` (message-count trimming only). See issue #223.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid_loop_trim_tokens: Option<usize>,
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
// Per-model tuning
// ---------------------------------------------------------------------------

/// Inference-parameter overrides for a specific model name.
///
/// Matched against the active model by exact string equality.  Add entries
/// under `[[model_tuning]]` in `~/.newt/config.toml` to pin parameters
/// for models whose defaults cause problems (e.g. context overflow).
///
/// Human-authored entries are never touched by the auto-tuner.  Auto-tuned
/// entries are appended (not modified) when the harness gains high confidence
/// in its empirical measurements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTuning {
    /// Model name as it appears in Ollama (e.g. `"nemotron3:33b"`).
    pub model: String,

    /// Ollama `options.num_ctx` — hard cap on KV-cache allocation.
    /// Overrides both the global `[tui].num_ctx` and the empirically
    /// derived `safe_context` from `model-capabilities.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,

    /// Per-model `mid_loop_trim_threshold` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid_loop_trim_threshold: Option<usize>,

    /// Per-model `mid_loop_trim_tokens` override (estimated-token trim trigger).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mid_loop_trim_tokens: Option<usize>,

    /// Per-model `max_tool_rounds` override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_rounds: Option<usize>,
}

impl Config {
    /// Find the first `[[model_tuning]]` entry whose `model` field matches
    /// `name` exactly.  Returns `None` when no entry exists.
    pub fn find_model_tuning(&self, name: &str) -> Option<&ModelTuning> {
        self.model_tuning.iter().find(|t| t.model == name)
    }
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
            mid_loop_trim_tokens: None,
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
            agents: AgentsConfig::default(),
            mcp_servers: Vec::new(),
            logs: None,
            skills: None,
            model_tuning: Vec::new(),
            conversations: None,
            merge: None,
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

    /// Resolve configuration by searching well-known locations, then layering a
    /// project-local override on top.
    ///
    /// Base search order (first match wins):
    /// 1. `$NEWT_CONFIG` environment variable
    /// 2. `./newt.toml`
    /// 3. `~/.newt/config.toml`
    /// 4. `/etc/newt/config.toml`
    ///
    /// Then, if a project-local `.newt/config.toml` is found by walking up from
    /// the current directory (see [`Config::project_config_path`]), it is
    /// deep-merged **over** the base so a repo can pin its own models, endpoints,
    /// rules, and local stdio MCP services without copying the whole global
    /// config. Tables merge recursively (project keys win) and scalars are
    /// replaced by the project value. Arrays follow `[merge] arrays` —
    /// `"replace"` (default) or `"append"` (see [`ArrayMergeStrategy`]). The
    /// project config's `[merge]` setting takes precedence, then the base's.
    /// See issue #222.
    ///
    /// When no project override exists this is byte-for-byte the legacy
    /// first-match behavior. Returns `Config::default()` if nothing is found.
    pub fn resolve() -> Result<Self> {
        let base_path = Self::candidate_paths().into_iter().find(|p| p.is_file());
        // A project-local config that *is* the base (e.g. cwd is the project and
        // its `.newt/config.toml` already matched) must not be merged onto itself.
        let project_path =
            Self::project_config_path().filter(|p| Some(p.as_path()) != base_path.as_deref());

        match (&base_path, &project_path) {
            // Fast path: no project override → exact legacy behavior.
            (Some(p), None) => Self::load(p),
            (None, None) => Ok(Self::default()),
            // Project override present → layer it over the base (or the default
            // config when there is no base file).
            (base, Some(proj)) => {
                let mut merged = match base {
                    Some(p) => Self::load_value(p)?,
                    None => toml::Value::try_from(Self::default())
                        .map_err(|e| NewtError::Config(e.to_string()))?,
                };
                let project_val = Self::load_value(proj)?;
                // The merge strategy is itself config: the project declares how
                // it wants to be merged (`[merge] arrays = ...`), else the global
                // config's setting, else the built-in default (Replace).
                let strategy = array_merge_strategy(&project_val, &merged);
                merge_toml(&mut merged, project_val, strategy);
                merged
                    .try_into()
                    .map_err(|e| NewtError::Config(e.to_string()))
            }
        }
    }

    /// Load a config file as a raw `toml::Value` (for layered merging).
    fn load_value(path: &Path) -> Result<toml::Value> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| NewtError::Config(e.to_string()))
    }

    /// Locate a project-local `.newt/config.toml` by walking up from the current
    /// directory toward the filesystem root, stopping before `$HOME` so the
    /// global `~/.newt/config.toml` is never mistaken for a project override.
    /// Returns the nearest match (innermost project wins). See issue #222.
    pub fn project_config_path() -> Option<PathBuf> {
        let cwd = std::env::current_dir().ok()?;
        find_project_config_from(&cwd, home_dir().as_deref())
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

/// Deep-merge `overlay` into `base`. Tables always merge recursively (overlay
/// keys win on collision). Arrays follow `arrays`: [`ArrayMergeStrategy::Replace`]
/// swaps the base array for the overlay's, [`ArrayMergeStrategy::Append`]
/// concatenates (base entries first). Scalars are always replaced by the
/// overlay. Used to layer a project-local `.newt/config.toml` over the global
/// config. See issue #222.
fn merge_toml(base: &mut toml::Value, overlay: toml::Value, arrays: ArrayMergeStrategy) {
    match (base, overlay) {
        (toml::Value::Table(base_tbl), toml::Value::Table(overlay_tbl)) => {
            for (key, val) in overlay_tbl {
                match base_tbl.get_mut(&key) {
                    Some(existing) => merge_toml(existing, val, arrays),
                    None => {
                        base_tbl.insert(key, val);
                    }
                }
            }
        }
        // Append mode: concatenate two arrays (global entries first).
        (toml::Value::Array(base_arr), toml::Value::Array(overlay_arr))
            if arrays == ArrayMergeStrategy::Append =>
        {
            base_arr.extend(overlay_arr);
        }
        // Replace mode (and any scalar): the overlay replaces the base outright.
        (slot, overlay) => *slot = overlay,
    }
}

/// Determine the array-merge strategy from the raw config values, before they
/// are deserialized. The project config expresses how *it* wants to be merged,
/// so it is consulted first; then the base config; else the built-in default.
fn array_merge_strategy(project: &toml::Value, base: &toml::Value) -> ArrayMergeStrategy {
    read_array_strategy(project)
        .or_else(|| read_array_strategy(base))
        .unwrap_or_default()
}

/// Read `[merge] arrays = "replace" | "append"` from a raw config value.
/// Returns `None` when the key is absent or unrecognized (caller falls back).
fn read_array_strategy(value: &toml::Value) -> Option<ArrayMergeStrategy> {
    match value.get("merge")?.get("arrays")?.as_str()? {
        "append" => Some(ArrayMergeStrategy::Append),
        "replace" => Some(ArrayMergeStrategy::Replace),
        _ => None,
    }
}

/// Walk up from `start` looking for a project-local `.newt/config.toml`,
/// stopping before `home` (so the global `~/.newt/config.toml` is never
/// returned) and at the filesystem root. Returns the innermost match.
///
/// Split out from [`Config::project_config_path`] so it can be unit-tested
/// against temp directories without mutating the process environment.
fn find_project_config_from(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(current) = dir {
        // Never treat the home directory's `.newt` as a project override.
        if home == Some(current) {
            break;
        }
        let candidate = current.join(".newt").join("config.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = current.parent();
    }
    None
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
    fn memory_note_nudge_interval_defaults_and_parses() {
        // Default: 10 — via Default and when `[memory]` omits the key.
        assert_eq!(MemoryConfig::default().note_nudge_interval, 10);
        let cfg: MemoryConfig = toml::from_str("provider = \"rolling_window\"").unwrap();
        assert_eq!(cfg.note_nudge_interval, 10);
        // 0 = nudge off.
        let cfg: MemoryConfig = toml::from_str("note_nudge_interval = 0").unwrap();
        assert_eq!(cfg.note_nudge_interval, 0);
    }

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
    fn conversations_config_defaults_to_count_cap() {
        let cfg = Config::default();
        let conversations = cfg.conversations.unwrap_or_default();
        assert_eq!(conversations.max_per_workspace, 100);
    }

    #[test]
    fn conversations_config_roundtrips_through_toml() {
        let cfg: Config = toml::from_str(
            r#"
[conversations]
max_per_workspace = 25
"#,
        )
        .unwrap();

        assert_eq!(cfg.conversations.unwrap_or_default().max_per_workspace, 25);
    }

    #[test]
    fn agents_config_default_enabled() {
        let cfg = AgentsConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.path, None);
        // A bare Config defaults agents to enabled too.
        assert!(Config::default().agents.enabled);
    }

    #[test]
    fn agents_config_roundtrips_with_path() {
        let cfg: Config = toml::from_str(
            r#"
[agents]
path = "docs/instructions"
"#,
        )
        .unwrap();
        assert!(cfg.agents.enabled);
        assert_eq!(cfg.agents.path.as_deref(), Some("docs/instructions"));

        // Serialize back out and confirm the path survives.
        let text = toml::to_string(&cfg).unwrap();
        assert!(text.contains("docs/instructions"));
    }

    #[test]
    fn agents_config_can_be_disabled() {
        let cfg: Config = toml::from_str(
            r#"
[agents]
enabled = false
"#,
        )
        .unwrap();
        assert!(!cfg.agents.enabled);
        assert_eq!(cfg.agents.path, None);
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

    // --- Project-local `.newt/config.toml` layering (issue #222) ---

    #[test]
    fn merge_toml_recurses_tables_and_replaces_scalars() {
        let mut base: toml::Value = toml::from_str(
            "a = 1\nb = 2\n[tui]\nmid_loop_trim_threshold = 40\nmax_tool_rounds = 25\n",
        )
        .unwrap();
        let overlay: toml::Value =
            toml::from_str("b = 99\nc = 3\n[tui]\nmax_tool_rounds = 5\n").unwrap();
        merge_toml(&mut base, overlay, ArrayMergeStrategy::Replace);
        // Scalar overridden, untouched scalar kept, new scalar added.
        assert_eq!(base["a"].as_integer(), Some(1));
        assert_eq!(base["b"].as_integer(), Some(99));
        assert_eq!(base["c"].as_integer(), Some(3));
        // Table merged recursively: overridden key wins, sibling preserved.
        assert_eq!(base["tui"]["max_tool_rounds"].as_integer(), Some(5));
        assert_eq!(
            base["tui"]["mid_loop_trim_threshold"].as_integer(),
            Some(40)
        );
    }

    #[test]
    fn merge_toml_replaces_arrays_wholesale_by_default() {
        let mut base: toml::Value = toml::from_str("models = [\"a\", \"b\", \"c\"]").unwrap();
        let overlay: toml::Value = toml::from_str("models = [\"x\"]").unwrap();
        merge_toml(&mut base, overlay, ArrayMergeStrategy::Replace);
        let arr = base["models"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "replace strategy swaps the array");
        assert_eq!(arr[0].as_str(), Some("x"));
    }

    #[test]
    fn merge_toml_appends_arrays_when_strategy_is_append() {
        let mut base: toml::Value = toml::from_str("models = [\"a\", \"b\"]").unwrap();
        let overlay: toml::Value = toml::from_str("models = [\"x\"]").unwrap();
        merge_toml(&mut base, overlay, ArrayMergeStrategy::Append);
        let arr = base["models"].as_array().unwrap();
        // Global entries first, then the project's appended.
        let got: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(got, vec!["a", "b", "x"]);
    }

    #[test]
    fn array_merge_strategy_project_wins_then_base_then_default() {
        let append: toml::Value = toml::from_str("[merge]\narrays = \"append\"\n").unwrap();
        let replace: toml::Value = toml::from_str("[merge]\narrays = \"replace\"\n").unwrap();
        let none: toml::Value = toml::from_str("x = 1").unwrap();
        // Project setting wins over the base.
        assert_eq!(
            array_merge_strategy(&append, &replace),
            ArrayMergeStrategy::Append
        );
        // Falls back to the base when the project is silent.
        assert_eq!(
            array_merge_strategy(&none, &append),
            ArrayMergeStrategy::Append
        );
        // Defaults to Replace when neither sets it.
        assert_eq!(
            array_merge_strategy(&none, &none),
            ArrayMergeStrategy::Replace
        );
        // Unrecognized values are ignored (fall through to default).
        let bogus: toml::Value = toml::from_str("[merge]\narrays = \"sideways\"\n").unwrap();
        assert_eq!(
            array_merge_strategy(&bogus, &none),
            ArrayMergeStrategy::Replace
        );
    }

    #[test]
    fn append_strategy_adds_project_mcp_server_to_global() {
        // The motivating case from issue #222: a project registers an extra
        // local stdio MCP server without redefining the global one.
        let global = "\
[merge]
arrays = \"append\"

[[mcp_servers]]
name = \"global-fs\"
command = \"mcp-fs\"
";
        let project = "\
[[mcp_servers]]
name = \"project-fs\"
command = \"mcp-fs\"
args = [\"--root\", \".\"]
";
        let mut merged: toml::Value = toml::from_str(global).unwrap();
        let proj_val: toml::Value = toml::from_str(project).unwrap();
        let strategy = array_merge_strategy(&proj_val, &merged);
        assert_eq!(strategy, ArrayMergeStrategy::Append);
        merge_toml(&mut merged, proj_val, strategy);
        let cfg: Config = merged.try_into().unwrap();
        let names: Vec<&str> = cfg.mcp_servers.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["global-fs", "project-fs"]);
    }

    #[test]
    fn find_project_config_walks_up_and_stops_before_home() {
        let home = tempfile::tempdir().unwrap();
        // home/proj/sub  with a project config at home/proj/.newt/config.toml
        let proj = home.path().join("proj");
        let sub = proj.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(proj.join(".newt")).unwrap();
        std::fs::write(proj.join(".newt").join("config.toml"), "x = 1").unwrap();
        // Also place a (global) config at home/.newt to prove it's NOT returned.
        std::fs::create_dir_all(home.path().join(".newt")).unwrap();
        std::fs::write(home.path().join(".newt").join("config.toml"), "x = 9").unwrap();

        let found = find_project_config_from(&sub, Some(home.path()));
        assert_eq!(found, Some(proj.join(".newt").join("config.toml")));

        // From a dir with no project config above it (but under home), nothing.
        let bare = home.path().join("empty");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(find_project_config_from(&bare, Some(home.path())), None);
    }

    #[test]
    fn project_config_deep_merges_over_global() {
        // global config: a backend + a tui block.
        let global = "\
[[backends]]
name = \"ollama\"
endpoint = \"http://localhost:11434\"
model = \"llama3\"
tiers = []
kind = \"ollama\"

[tui]
mid_loop_trim_threshold = 40
max_tool_rounds = 25
";
        // project override: change max_tool_rounds only.
        let project = "[tui]\nmax_tool_rounds = 7\n";

        let mut merged: toml::Value = toml::from_str(global).unwrap();
        merge_toml(
            &mut merged,
            toml::from_str(project).unwrap(),
            ArrayMergeStrategy::Replace,
        );
        let cfg: Config = merged.try_into().unwrap();

        // Overridden value wins…
        assert_eq!(cfg.tui.as_ref().unwrap().max_tool_rounds, 7);
        // …sibling key preserved from global…
        assert_eq!(cfg.tui.as_ref().unwrap().mid_loop_trim_threshold, 40);
        // …and the global backend survived (not in the override).
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

    #[test]
    fn model_tuning_parses_from_toml() {
        let toml = r#"
            [[model_tuning]]
            model = "nemotron3:33b"
            num_ctx = 24576
            mid_loop_trim_threshold = 12
            max_tool_rounds = 20

            [[model_tuning]]
            model = "qwen3-coder:30b"
            num_ctx = 65536
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.model_tuning.len(), 2);

        let nemo = cfg.find_model_tuning("nemotron3:33b").unwrap();
        assert_eq!(nemo.num_ctx, Some(24576));
        assert_eq!(nemo.mid_loop_trim_threshold, Some(12));
        assert_eq!(nemo.max_tool_rounds, Some(20));

        let qwen = cfg.find_model_tuning("qwen3-coder:30b").unwrap();
        assert_eq!(qwen.num_ctx, Some(65536));
        assert_eq!(qwen.mid_loop_trim_threshold, None);
    }

    #[test]
    fn model_tuning_find_returns_none_for_unknown_model() {
        let cfg = Config::default();
        assert!(cfg.find_model_tuning("nonexistent:7b").is_none());
    }

    #[test]
    fn model_tuning_partial_fields_are_optional() {
        let toml = r#"
            [[model_tuning]]
            model = "llama3.1:8b"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let entry = cfg.find_model_tuning("llama3.1:8b").unwrap();
        assert_eq!(entry.num_ctx, None);
        assert_eq!(entry.mid_loop_trim_threshold, None);
        assert_eq!(entry.max_tool_rounds, None);
    }
}
