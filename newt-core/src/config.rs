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

    /// Named permission presets (`[permission_presets.<name>]`, issue #307).
    /// Each maps onto the role-profile caveat mechanism (a
    /// [`crate::NamedPermissionPreset`]) and, when applied via `/mode`, clamps
    /// the session's authority as a hard floor. Empty by default — no preset,
    /// behavior unchanged.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub permission_presets: std::collections::BTreeMap<String, crate::NamedPermissionPreset>,

    /// Named modes (`[modes.<name>]`, issue #307) for the `/mode` command. Each
    /// mode atomically binds a skill body to preload, a permission preset to
    /// apply as an authority floor, and a one-line system-prompt framing. Empty
    /// by default. See [`ModeConfig`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub modes: std::collections::BTreeMap<String, ModeConfig>,

    /// Named profiles (`[profiles.<name>]`) — a composition of harness
    /// *techniques* plus each technique's tunable knob settings (the technique
    /// library, `docs/design/technique-library.md`). A profile is selected by
    /// `--profile <name>` and tunes the harness per model family / context.
    /// Empty by default — no profile, behavior unchanged. See [`ProfileConfig`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub profiles: std::collections::BTreeMap<String, ProfileConfig>,

    /// Named bundles (`[bundles.<name>]`) — the loadable unit of the model support
    /// kit (`docs/design/model-support-kit.md`). A bundle pins which model families
    /// it applies to and which profile each resolves to. Selected by `--bundle
    /// <name>` or inferred from the model via `applies_to`. Empty by default — no
    /// bundle, behavior unchanged. See [`BundleConfig`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub bundles: std::collections::BTreeMap<String, BundleConfig>,

    /// Named loadouts (`[loadouts.<name>]` or `~/.newt/loadouts/<name>.toml`) — the
    /// top-level composition of `provider → model → kit → role → settings`
    /// (`docs/design/loadout-composition.md`). Inert until the resolver is wired
    /// (Slice 1): this carries the data model + reference validation + `/loadout`
    /// show. Empty by default. See [`Loadout`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub loadouts: std::collections::BTreeMap<String, Loadout>,
}

/// One named mode (`[modes.<name>]`, issue #307): the atomic binding the
/// `/mode <name>` command applies in a single invocation.
///
/// ```toml
/// [modes.triage]
/// skill   = "oncall-triage"        # skill body to preload (use_skill path)
/// preset  = "readonly-triage"      # [permission_presets.<name>] to clamp to
/// framing = "On-call triage: investigate, do not change production."
/// ```
///
/// Every field is optional so a mode can do any subset (e.g. preset-only, or
/// framing-only). A `skill`/`preset` that names a missing entry is reported as
/// an error by the command rather than silently ignored — a mode that claims a
/// clamp it never applied would be a false security claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ModeConfig {
    /// Skill name to preload (the same `use_skill` / `load_body_from` path).
    /// `None` ⇒ no skill is loaded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// `[permission_presets.<name>]` to apply as the session authority floor.
    /// `None` ⇒ authority unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// One-line framing injected into the system prompt. `None` ⇒ no framing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framing: Option<String>,
}

/// The known harness techniques a profile may compose — the registry the
/// validator checks against. A profile naming a technique outside this set is
/// rejected (an unknown technique a profile claims but cannot apply would be a
/// false claim). Extend this as techniques land (R3 `fact_preserving_compression`,
/// R4 `self_grounding`, …).
pub const KNOWN_TECHNIQUES: &[&str] = &[
    "knowledge_base", // R1 — inject the authoritative import surface (#74)
    "verify_gate",    // R2 — revert files with fabricated imports (#73)
    "retry",          // revert-retry loop over the gate's revert set
];

/// One named profile (`[profiles.<name>]`): the harness techniques to compose for
/// a model family / context, plus each technique's knob settings.
///
/// ```toml
/// [profiles.nemotron]
/// techniques = ["knowledge_base", "verify_gate", "retry"]
///
/// [profiles.nemotron.verify_gate]
/// surface_match = "exact"        # SurfaceMatch — leaf-exact (the complete-gate default)
///
/// [profiles.nemotron.retry]
/// max_retries = 2
/// ```
///
/// A knob table only takes effect when its technique is enabled. An unknown
/// technique name is an error ([`ProfileConfig::validate`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ProfileConfig {
    /// The ordered set of techniques this profile composes. Empty ⇒ the profile
    /// applies no techniques (equivalent to the `default`/light profile).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub techniques: Vec<String>,
    /// Knobs for the `verify_gate` technique (applied iff it is enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_gate: Option<VerifyGateKnobs>,
    /// Knobs for the `retry` technique (applied iff it is enabled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<RetryKnobs>,
}

/// Tunable knobs for the `verify_gate` technique.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct VerifyGateKnobs {
    /// How strictly the project surface is matched. Default `Exact` — the
    /// adversarially-complete setting (the retry-Goodhart finding).
    #[serde(default)]
    pub surface_match: crate::verify_gate::SurfaceMatch,
}

/// Tunable knobs for the `retry` technique.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryKnobs {
    /// Maximum revert-retry attempts. Default 2.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

const fn default_max_retries() -> u32 {
    2
}

impl Default for RetryKnobs {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
        }
    }
}

impl ProfileConfig {
    /// Validate the profile against the [component registry](crate::kit): every
    /// named technique must be a known component, and every component's
    /// `presupposes` must also be enabled (e.g. `retry` presupposes `verify_gate`).
    /// A presupposition gap is a **load-time** error, not a silent partial apply.
    ///
    /// # Errors
    /// Returns the first unknown-technique or unmet-presupposition as a message.
    pub fn validate(&self) -> std::result::Result<(), String> {
        for t in &self.techniques {
            let Some(entry) = crate::kit::component(t) else {
                return Err(format!(
                    "unknown technique '{t}' in profile (known: {})",
                    KNOWN_TECHNIQUES.join(", ")
                ));
            };
            for pre in entry.presupposes {
                if !self.techniques.iter().any(|x| x == pre) {
                    return Err(format!(
                        "technique '{t}' presupposes '{pre}', which the profile does not enable"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Whether this profile enables `technique`.
    #[must_use]
    pub fn enables(&self, technique: &str) -> bool {
        self.techniques.iter().any(|t| t == technique)
    }

    /// The effective `verify_gate` knobs (defaults when unset).
    #[must_use]
    pub fn verify_gate_knobs(&self) -> VerifyGateKnobs {
        self.verify_gate.unwrap_or_default()
    }

    /// The effective `retry` knobs (defaults when unset).
    #[must_use]
    pub fn retry_knobs(&self) -> RetryKnobs {
        self.retry.unwrap_or_default()
    }
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

    /// Auto-resume this workspace's most recently active conversation at TUI
    /// session start (Step 17.7, issue #246). "Most recently active" means
    /// the highest §6 activity tick — never a wall-clock comparison.
    ///
    /// **Default: true.** The off-switch:
    ///
    /// ```toml
    /// [conversations]
    /// resume = false      # always start fresh
    /// ```
    ///
    /// Per-session overrides win over this key either way: `--ephemeral`
    /// (no persistence at all) and `NEWT_CONVERSATION_ID=<id>` (resume
    /// exactly that conversation).
    #[serde(default = "default_conversations_resume")]
    pub resume: bool,
}

fn default_conversations_max_per_workspace() -> usize {
    100
}

fn default_conversations_resume() -> bool {
    true
}

impl Default for ConversationsConfig {
    fn default() -> Self {
        Self {
            max_per_workspace: default_conversations_max_per_workspace(),
            resume: default_conversations_resume(),
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

    /// End-of-conversation note extraction (Step 19.4, #248): when `true`,
    /// closing a conversation (`/new` or a clean exit) runs ONE synchronous
    /// tools-disabled completion that distills at most 3 durable facts into
    /// NOTES.md through the scanned `save_note` write path. Default: `false`
    /// — the pass is optional and costs one completion per close.
    #[serde(default)]
    pub extract_notes_on_close: bool,

    /// How memory is disclosed to the model (progressive-disclosure memory,
    /// Workstream A MVP, #319). `Frozen` (the default) is today's behavior
    /// exactly: NOTES are frozen verbatim into the system prompt and the
    /// `memory_fetch` tool is not wired. `Index` opts in to the budgeted
    /// memory INDEX (note titles/ids instead of full bodies) plus the
    /// `memory_fetch` tool that pulls a body on demand. This is a context-cost
    /// facet, never an authorization knob.
    #[serde(default)]
    pub disclosure: MemoryDisclosure,
}

/// Memory disclosure mode — the `[memory] disclosure` key (#319).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MemoryDisclosure {
    /// Today's behavior: NOTES frozen verbatim into the system prompt, no
    /// `memory_fetch` tool. The MVP default — inert unless opted in.
    #[default]
    Frozen,
    /// Progressive disclosure: a budgeted memory INDEX in the prompt plus the
    /// `memory_fetch` tool to pull bodies on demand.
    Index,
}

/// Prompt richness — the `[tui] footer` key. Selects the *default* prompt
/// template when `[tui] prompt` is unset; an explicit `[tui] prompt` always
/// wins. The rich default folds a timestamp + status into the prompt line
/// itself (`[<ts> · <model> · <ws> · <mode> ] ❯ `), so rustyline floats it at
/// the bottom while idle (like cargo's progress line) and it doubles as a
/// greppable per-turn log marker — no region, no cursor games.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FooterMode {
    /// Rich default prompt on a TTY, plain `\w $ ` otherwise (the default).
    /// The amphibious choice: decorated on a human terminal, bare in pipes /
    /// `newt worker` / the wyvern deep-cut.
    #[default]
    Auto,
    /// Always use the rich default prompt (even off a TTY — screenshots, tests).
    On,
    /// Always use the plain bare prompt. Equivalent to `--plain`.
    Off,
}

/// How a thinking model's streamed reasoning is surfaced — the `[tui] thinking`
/// key. Newt strips `<think>…</think>` from the reply regardless (#385); this
/// only controls the live human display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    /// Cargo-style: reasoning streams as dim scrolled lines (kept in
    /// scrollback) with an ephemeral spinner line pinned at the bottom. The
    /// default — but only on a TTY; a pipe / `newt worker` shows nothing.
    #[default]
    Stream,
    /// No reasoning display at all (the answer still streams normally).
    Off,
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
            extract_notes_on_close: false,
            disclosure: MemoryDisclosure::Frozen,
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

    /// Input-footer mode: the transient multi-line `❯` input block with a
    /// status header. `"auto"` (default) shows it on a TTY and degrades to a
    /// plain scroller otherwise; `"on"` always shows it; `"off"` never does
    /// (the `--plain` CLI flag, or `NEWT_FOOTER=off`).
    #[serde(default)]
    pub footer: FooterMode,

    /// How a thinking model's streamed reasoning is shown: `"stream"` (default
    /// — dim reasoning + a cargo-style spinner, TTY only) or `"off"`.
    #[serde(default)]
    pub thinking: ThinkingMode,

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

    /// Enable deep backend/inference diagnostics. Intended for issue reports
    /// and compatibility debugging; also set via `NEWT_TRACE=1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<bool>,

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

    /// Normalise hyphens to underscores in MCP server names when advertising
    /// tool definitions and routing tool calls.  Some API proxies (e.g. those
    /// that wrap the Anthropic backend) replace hyphens with underscores in
    /// tool names; advertising the sanitised form ensures the model's tool
    /// calls round-trip back unchanged and routes correctly.  Default: `true`.
    /// Set to `false` only when every connected MCP server is behind a proxy
    /// that preserves hyphens verbatim.
    #[serde(default = "default_sanitize_mcp_server_names")]
    pub sanitize_mcp_server_names: bool,

    /// Hosts (IP or hostname) for which newt may send an MCP OAuth Bearer token
    /// over an UNENCRYPTED (non-`https`) connection. Empty by default: a stored
    /// Bearer is sent only over `https` or to loopback (`localhost`/`127.0.0.1`/
    /// `::1`). newt WARNs on every non-loopback unencrypted MCP connection
    /// regardless; an allow-listed host still warns but the token is sent.
    /// This is the explicit opt-out of the secure-by-default transport policy —
    /// see `docs/decisions/mcp_transport_security.md`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_allow_insecure_hosts: Vec<String>,
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

fn default_sanitize_mcp_server_names() -> bool {
    true
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

    /// Look up and validate a named profile (`[profiles.<name>]`). The caller
    /// selects it via `--profile <name>` / `NEWT_PROFILE`.
    ///
    /// # Errors
    /// `no such profile` when the name is undefined; the validation error when
    /// the profile names an unknown technique — a `--profile` that silently did
    /// nothing would be a false claim, so both fail loudly.
    pub fn resolve_profile(&self, name: &str) -> std::result::Result<&ProfileConfig, String> {
        let profile = self.profiles.get(name).ok_or_else(|| {
            let known = if self.profiles.is_empty() {
                "none defined".to_string()
            } else {
                self.profiles.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            format!("no such profile (known: {known})")
        })?;
        profile.validate()?;
        Ok(profile)
    }

    /// Look up a named bundle (`[bundles.<name>]`).
    ///
    /// # Errors
    /// `no such bundle` when undefined — a `--bundle` that silently did nothing
    /// would be a false claim.
    pub fn resolve_bundle(&self, name: &str) -> std::result::Result<&BundleConfig, String> {
        self.bundles.get(name).ok_or_else(|| {
            let known = if self.bundles.is_empty() {
                "none defined".to_string()
            } else {
                self.bundles.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            format!("no such bundle (known: {known})")
        })
    }

    /// The profile name `bundle` yields for `model`: the longest-prefix `families`
    /// match, else `default_profile`. `None` ⇒ the bundle applies no profile here.
    #[must_use]
    pub fn bundle_profile_for_model<'a>(
        &self,
        bundle: &'a BundleConfig,
        model: &str,
    ) -> Option<&'a str> {
        bundle
            .families
            .iter()
            .filter(|(prefix, _)| model.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len()) // longest-prefix-wins
            .map(|(_, p)| p.as_str())
            .or(bundle.default_profile.as_deref())
    }

    /// Infer the bundle for `model` from `applies_to` (longest-prefix-wins). Only
    /// bundles with a non-empty `applies_to` participate — a use-case bundle (empty
    /// `applies_to`) is never auto-inferred, only chosen explicitly via `--bundle`.
    #[must_use]
    pub fn infer_bundle(&self, model: &str) -> Option<(&str, &BundleConfig)> {
        self.bundles
            .iter()
            .filter_map(|(name, b)| {
                b.applies_to
                    .iter()
                    .filter(|p| model.starts_with(p.as_str()))
                    .map(String::len)
                    .max()
                    .map(|best| (best, name.as_str(), b))
            })
            .max_by_key(|(best, _, _)| *best)
            .map(|(_, name, b)| (name, b))
    }

    /// Resolve the active profile from the selectors + the active `model`:
    /// `--profile` (explicit) > `--bundle` (its profile for this model) > an
    /// inferred bundle (`applies_to`) > `None` (today's no-profile behavior).
    /// Returns the profile NAME + how it was chosen (for the banner).
    ///
    /// # Errors
    /// An unknown explicit `--bundle` is a hard error. An unknown explicit
    /// `--profile` is left for the caller's [`resolve_profile`](Self::resolve_profile)
    /// so the message stays profile-specific.
    pub fn pick_active_profile(
        &self,
        profile_flag: Option<&str>,
        bundle_flag: Option<&str>,
        model: &str,
    ) -> std::result::Result<Option<ProfilePick>, String> {
        if let Some(p) = profile_flag.filter(|s| !s.is_empty()) {
            return Ok(Some(ProfilePick {
                name: p.to_string(),
                via: PickVia::Profile,
            }));
        }
        if let Some(b) = bundle_flag.filter(|s| !s.is_empty()) {
            let bundle = self.resolve_bundle(b)?;
            return Ok(self
                .bundle_profile_for_model(bundle, model)
                .map(|p| ProfilePick {
                    name: p.to_string(),
                    via: PickVia::Bundle(b.to_string()),
                }));
        }
        if let Some((name, bundle)) = self.infer_bundle(model) {
            return Ok(self
                .bundle_profile_for_model(bundle, model)
                .map(|p| ProfilePick {
                    name: p.to_string(),
                    via: PickVia::InferredBundle(name.to_string()),
                }));
        }
        Ok(None)
    }
}

/// One named bundle (`[bundles.<name>]`) — the loadable unit of the model support
/// kit. It pins which model families it applies to and which profile each resolves
/// to, shipping the `[profiles.*]` it references.
///
/// ```toml
/// [bundles.nemotron]
/// about = "Support bundle for the nemotron family"
/// applies_to = ["nemotron"]                 # longest-prefix-wins; "nemotron3:33b" matches
/// default_profile = "nemotron"
/// families = { "nemotron" = "nemotron", "qwen" = "qwen-coder" }
/// ```
///
/// A bundle carries **no authority** — there is deliberately no caveats/preset
/// field; it recombines vetted parts, it cannot grant (`docs/design/model-support-kit.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BundleConfig {
    /// One-line provenance, shown in the startup banner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    /// Model-id prefixes this bundle auto-applies to (longest-prefix-wins). Empty ⇒
    /// a use-case bundle: chosen only via explicit `--bundle`, never auto-inferred.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applies_to: Vec<String>,
    /// Profile applied when this bundle is selected and no `families` entry matches.
    /// Must name a key in `[profiles.*]`. `None` ⇒ no profile (the light path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<String>,
    /// model-family-prefix → profile name (longest-prefix-wins over `default_profile`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub families: std::collections::BTreeMap<String, String>,
}

/// The active-profile selection + how it was chosen (for honest banner output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfilePick {
    /// The chosen profile name (to feed [`Config::resolve_profile`]).
    pub name: String,
    /// Which selector won.
    pub via: PickVia,
}

/// How a [`ProfilePick`] was selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickVia {
    /// An explicit `--profile` / `NEWT_PROFILE`.
    Profile,
    /// An explicit `--bundle <name>`.
    Bundle(String),
    /// A bundle inferred from the model via `applies_to`.
    InferredBundle(String),
}

/// One named loadout (`[loadouts.<name>]` / `~/.newt/loadouts/<name>.toml`) — the
/// top-level composition the user *loads* (`docs/design/loadout-composition.md`).
/// Every field is optional and is a **name reference** into the surface that owns
/// that axis; the loadout itself stores nothing but the selection + per-axis
/// overrides. It carries **no authority** — `settings` cannot widen caveats.
///
/// ```toml
/// [loadouts.dev-nemotron]
/// provider = "dgx"          # → the catalog/provider card (#387)
/// model    = "nemotron@deep"
/// kit      = "nemotron"     # → a [bundles.<name>] (the loadable kit unit)
/// profile  = "nemotron"     # → a [profiles.<name>] (optional; the bundle implies it)
/// role     = "python-developer"   # → ~/.newt/personas/<name>.md
///   [loadouts.dev-nemotron.settings]
///   num_ctx = 24576
///   framing = "Ship small, verify."
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Loadout {
    /// Provider id (→ the catalog/provider card). Resolution is Slice 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model id, optionally `model@variant`. Resolution is Slice 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Bundle name (the loadable kit unit) — must name a `[bundles.<name>]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kit: Option<String>,
    /// Profile name — must name a `[profiles.<name>]`. Omitted ⇒ the bundle/model
    /// implies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Role/persona name (`~/.newt/personas/<name>.md`). Not validated against the
    /// filesystem here — personas are resolved at session start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Per-axis overrides (parameters / prompt). Never authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<LoadoutSettings>,
}

/// Per-axis overrides a loadout may pin. **No authority axis** — a loadout cannot
/// widen caveats (`docs/design/loadout-composition.md` §Authority safety).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoadoutSettings {
    /// Parameter axis: KV-cache window override (top of the `ModelTuning` chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    /// Prompt axis: a one-line system-prompt framing (the `ModeConfig.framing` shape).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub framing: Option<String>,
}

impl Loadout {
    /// Validate the loadout's name references against `cfg`: a named `kit` must be a
    /// known bundle, a named `profile` must be a known, valid profile, and a named
    /// `provider` must name a `[backends]` entry (Slice 2 — the provider/model axis).
    /// A dangling reference is a hard error — a loadout that silently did nothing
    /// would be a false claim. The `@variant` half of `model` and `role` are resolved
    /// by their own surfaces later and are not checked here.
    ///
    /// # Errors
    /// The first dangling `kit`, `profile`, or `provider` reference, as a message.
    pub fn validate(&self, cfg: &Config) -> std::result::Result<(), String> {
        if let Some(kit) = &self.kit {
            cfg.resolve_bundle(kit)
                .map_err(|e| format!("loadout kit '{kit}': {e}"))?;
        }
        if let Some(profile) = &self.profile {
            cfg.resolve_profile(profile)
                .map_err(|e| format!("loadout profile '{profile}': {e}"))?;
        }
        if let Some(provider) = &self.provider {
            if !cfg.backends.iter().any(|b| &b.name == provider) {
                let known = if cfg.backends.is_empty() {
                    "none defined".to_string()
                } else {
                    cfg.backends
                        .iter()
                        .map(|b| b.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                return Err(format!(
                    "loadout provider '{provider}': no [backends] entry named '{provider}' (known: {known})"
                ));
            }
        }
        Ok(())
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

    /// Prompt the human when a tool call is denied by the session's caveats
    /// (issue #263): allow once / allow for this session / deny. Default
    /// **false** — a denial fails the call exactly as before (deny-by-default
    /// stays the posture). Interactive TUI only; headless paths (ACP worker,
    /// `newt-eval`) never prompt regardless. Also enabled per-run via the
    /// `--prompt-for-permissions` CLI flag. Every prompted decision is
    /// recorded to `~/.newt/permission-log.jsonl` for later review.
    #[serde(default)]
    pub prompt: bool,
}

impl Default for ToolPermissions {
    fn default() -> Self {
        Self {
            preset: PermissionPreset::WorkspaceDev,
            extra_exec: Vec::new(),
            net: Vec::new(),
            prompt: false,
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
            footer: FooterMode::Auto,
            thinking: ThinkingMode::Stream,
            tool_output_lines: default_tool_output_lines(),
            max_tool_rounds: default_max_tool_rounds(),
            permissions: ToolPermissions::default(),
            debug: None,
            trace: None,
            build_check_cmd: None,
            num_ctx: None,
            connect_timeout_secs: default_connect_timeout_secs(),
            inference_timeout_secs: default_inference_timeout_secs(),
            keep_alive: default_keep_alive(),
            mid_loop_trim_threshold: default_mid_loop_trim_threshold(),
            mid_loop_trim_tokens: None,
            sanitize_mcp_server_names: default_sanitize_mcp_server_names(),
            mcp_allow_insecure_hosts: Vec::new(),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
            permission_presets: std::collections::BTreeMap::new(),
            modes: std::collections::BTreeMap::new(),
            profiles: std::collections::BTreeMap::new(),
            bundles: std::collections::BTreeMap::new(),
            loadouts: std::collections::BTreeMap::new(),
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

        let mut cfg = match (&base_path, &project_path) {
            // Fast path: no project override → exact legacy behavior.
            (Some(p), None) => Self::load(p)?,
            (None, None) => Self::default(),
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
                    .map_err(|e| NewtError::Config(e.to_string()))?
            }
        };
        // Per-file bundles (the model-support-kit control surface): drop a
        // `~/.newt/bundles/<name>.toml` to add a bundle — no `config.toml` edit.
        cfg.merge_disk_bundles();
        // Per-file loadouts (the shareable composition control surface): drop a
        // `~/.newt/loadouts/<name>.toml` to add a loadout — no `config.toml` edit.
        // Runs after bundles so a disk loadout may name a disk bundle.
        cfg.merge_disk_loadouts();
        Ok(cfg)
    }

    /// Merge per-file bundles from the well-known `bundles/` dirs next to the
    /// config: `~/.newt/bundles/*.toml` first, then the project `.newt/bundles/`
    /// (so project overrides home overrides inline `[bundles.*]`). The filename
    /// stem is the bundle name. A malformed drop-in is skipped with a warning — it
    /// must not break startup.
    fn merge_disk_bundles(&mut self) {
        if let Some(h) = home_dir() {
            self.merge_bundles_from_dir(&h.join(".newt").join("bundles"));
        }
        if let Some(proj) = Self::project_config_path() {
            if let Some(parent) = proj.parent() {
                self.merge_bundles_from_dir(&parent.join("bundles"));
            }
        }
    }

    /// Load `<dir>/*.toml` as bundles (filename stem = name) into `self.bundles`,
    /// last-wins on a name clash. A malformed file is skipped with a warning.
    fn merge_bundles_from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // no bundles dir — fine
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        paths.sort();
        for path in paths {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match std::fs::read_to_string(&path).map(|t| toml::from_str::<BundleConfig>(&t)) {
                Ok(Ok(bundle)) => {
                    self.bundles.insert(stem.to_string(), bundle);
                }
                Ok(Err(e)) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping malformed bundle file");
                }
                Err(_) => {}
            }
        }
    }

    /// Merge per-file loadouts from the well-known `loadouts/` dirs next to the
    /// config: `~/.newt/loadouts/*.toml` first, then the project `.newt/loadouts/`
    /// (so project overrides home overrides inline `[loadouts.*]`). The filename
    /// stem is the loadout name. A malformed drop-in is skipped with a warning — it
    /// must not break startup. References *inside* a loadout are validated when it
    /// is selected (`--loadout`), not at load, mirroring the inline `[loadouts.*]`
    /// path.
    fn merge_disk_loadouts(&mut self) {
        if let Some(h) = home_dir() {
            self.merge_loadouts_from_dir(&h.join(".newt").join("loadouts"));
        }
        if let Some(proj) = Self::project_config_path() {
            if let Some(parent) = proj.parent() {
                self.merge_loadouts_from_dir(&parent.join("loadouts"));
            }
        }
    }

    /// Load `<dir>/*.toml` as loadouts (filename stem = name) into `self.loadouts`,
    /// last-wins on a name clash. A malformed file is skipped with a warning.
    fn merge_loadouts_from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // no loadouts dir — fine
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        paths.sort();
        for path in paths {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            match std::fs::read_to_string(&path).map(|t| toml::from_str::<Loadout>(&t)) {
                Ok(Ok(loadout)) => {
                    self.loadouts.insert(stem.to_string(), loadout);
                }
                Ok(Err(e)) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping malformed loadout file");
                }
                Err(_) => {}
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

    /// Serialize the config to pretty TOML for **audit**, with inline secret
    /// material redacted. The values of every `[[mcp_servers]]` `env` and
    /// `headers` entry are replaced with [`Self::REDACTED`] — those maps are the
    /// only place `Config` can carry a raw secret inline (e.g. an
    /// `Authorization: Bearer …` header or an `API_KEY=…` child env var). Keys
    /// are kept so an auditor sees *which* variables/headers are set without the
    /// values. Secret *references* (`api_key_file` / `api_key_env`) are left as-is
    /// — they name where a secret lives, not the secret itself.
    ///
    /// # Errors
    /// A TOML serialization failure (should not happen for a valid `Config`).
    pub fn to_redacted_toml(&self) -> Result<String> {
        let mut redacted = self.clone();
        for server in &mut redacted.mcp_servers {
            for v in server.env.values_mut() {
                *v = Self::REDACTED.to_string();
            }
            for v in server.headers.values_mut() {
                *v = Self::REDACTED.to_string();
            }
        }
        toml::to_string_pretty(&redacted).map_err(|e| NewtError::Config(e.to_string()))
    }

    /// Placeholder substituted for redacted secret values in [`Self::to_redacted_toml`].
    pub const REDACTED: &'static str = "<redacted>";

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
pub(crate) fn home_dir() -> Option<PathBuf> {
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
pub(crate) fn merge_toml(base: &mut toml::Value, overlay: toml::Value, arrays: ArrayMergeStrategy) {
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
pub(crate) fn find_project_config_from(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
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
pub(crate) fn expand_tilde(path: &str) -> PathBuf {
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

    // ── input-footer mode ──────────────────────────────────────────────

    #[test]
    fn footer_mode_defaults_to_auto_and_round_trips() {
        // Absent key → Auto (the amphibious default).
        let cfg: TuiConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.footer, FooterMode::Auto);
        // Each variant parses from its snake_case key.
        for (key, want) in [
            ("auto", FooterMode::Auto),
            ("on", FooterMode::On),
            ("off", FooterMode::Off),
        ] {
            let cfg: TuiConfig = toml::from_str(&format!("footer = \"{key}\"")).unwrap();
            assert_eq!(cfg.footer, want, "footer = {key}");
        }
    }

    #[test]
    fn thinking_mode_defaults_to_stream_and_round_trips() {
        let cfg: TuiConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.thinking, ThinkingMode::Stream);
        let cfg: TuiConfig = toml::from_str("thinking = \"off\"").unwrap();
        assert_eq!(cfg.thinking, ThinkingMode::Off);
        let cfg: TuiConfig = toml::from_str("thinking = \"stream\"").unwrap();
        assert_eq!(cfg.thinking, ThinkingMode::Stream);
    }

    // ── profile composition (technique library) ────────────────────────

    #[test]
    fn profile_parses_techniques_and_knobs() {
        let cfg: Config = toml::from_str(
            r#"
            [profiles.nemotron]
            techniques = ["knowledge_base", "verify_gate", "retry"]

            [profiles.nemotron.verify_gate]
            surface_match = "exact"

            [profiles.nemotron.retry]
            max_retries = 3
            "#,
        )
        .unwrap();
        let p = &cfg.profiles["nemotron"];
        assert!(p.validate().is_ok());
        assert!(p.enables("verify_gate") && p.enables("retry"));
        assert_eq!(
            p.verify_gate_knobs().surface_match,
            crate::verify_gate::SurfaceMatch::Exact
        );
        assert_eq!(p.retry_knobs().max_retries, 3);
    }

    #[test]
    fn profile_knobs_default_when_unset() {
        // techniques named but no knob tables → defaults apply
        let p: ProfileConfig = toml::from_str("techniques = [\"verify_gate\", \"retry\"]").unwrap();
        assert_eq!(
            p.verify_gate_knobs().surface_match,
            crate::verify_gate::SurfaceMatch::Exact // the complete-gate default
        );
        assert_eq!(p.retry_knobs().max_retries, 2);
    }

    #[test]
    fn profile_rejects_unknown_technique() {
        let p: ProfileConfig =
            toml::from_str("techniques = [\"knowledge_base\", \"teleport\"]").unwrap();
        let err = p.validate().unwrap_err();
        assert!(err.contains("teleport"), "err: {err}");
    }

    #[test]
    fn profile_rejects_unmet_presupposition() {
        // retry presupposes verify_gate — listing retry alone is now a load-time error.
        let p: ProfileConfig = toml::from_str("techniques = [\"retry\"]").unwrap();
        let err = p.validate().unwrap_err();
        assert!(
            err.contains("retry") && err.contains("verify_gate") && err.contains("presupposes"),
            "err: {err}"
        );
        // …and adding verify_gate satisfies it.
        let ok: ProfileConfig =
            toml::from_str("techniques = [\"verify_gate\", \"retry\"]").unwrap();
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn registry_does_not_alter_the_resolved_technique_set() {
        // Golden: validate() accepts the nemotron set and the resolved order/membership
        // is byte-identical to the input — the registry adds checks, not behavior.
        let p: ProfileConfig =
            toml::from_str("techniques = [\"knowledge_base\", \"verify_gate\", \"retry\"]")
                .unwrap();
        assert!(p.validate().is_ok());
        assert_eq!(p.techniques, vec!["knowledge_base", "verify_gate", "retry"]);
        for t in ["knowledge_base", "verify_gate", "retry"] {
            assert!(p.enables(t));
        }
    }

    #[test]
    fn empty_profiles_is_the_default() {
        // no [profiles] table → empty map, behavior unchanged
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.profiles.is_empty());
        assert!(cfg.bundles.is_empty());
    }

    // ── bundles (the loadable kit unit) ────────────────────────────────

    fn bundle_cfg() -> Config {
        toml::from_str(
            r#"
            [profiles.nemotron]
            techniques = ["knowledge_base", "verify_gate", "retry"]
            [profiles.qwen-coder]
            techniques = []

            [bundles.nemotron]
            about = "nemotron family support"
            applies_to = ["nemotron"]
            default_profile = "nemotron"
            families = { "nemotron" = "nemotron", "qwen" = "qwen-coder" }

            [bundles.review-heavy]              # use-case bundle: no applies_to
            default_profile = "nemotron"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn resolve_bundle_errors_on_unknown() {
        let cfg = bundle_cfg();
        assert!(cfg.resolve_bundle("nemotron").is_ok());
        let err = cfg.resolve_bundle("ghost").unwrap_err();
        assert!(err.contains("no such bundle"), "{err}");
    }

    #[test]
    fn bundle_profile_for_model_longest_prefix_then_default() {
        let cfg = bundle_cfg();
        let b = cfg.resolve_bundle("nemotron").unwrap();
        // family-prefix match
        assert_eq!(
            cfg.bundle_profile_for_model(b, "nemotron3:33b"),
            Some("nemotron")
        );
        assert_eq!(
            cfg.bundle_profile_for_model(b, "qwen2.5-coder"),
            Some("qwen-coder")
        );
        // no family match → default_profile
        assert_eq!(
            cfg.bundle_profile_for_model(b, "llama3.1:8b"),
            Some("nemotron")
        );
    }

    #[test]
    fn infer_bundle_only_from_applies_to() {
        let cfg = bundle_cfg();
        // nemotron model → the nemotron bundle (applies_to match)
        assert_eq!(
            cfg.infer_bundle("nemotron3:33b").map(|(n, _)| n),
            Some("nemotron")
        );
        // a model no applies_to matches → no inference (the use-case bundle is never inferred)
        assert!(cfg.infer_bundle("gpt-4.1").is_none());
    }

    #[test]
    fn pick_active_profile_precedence() {
        let cfg = bundle_cfg();
        // 1. explicit --profile wins over everything
        let p = cfg
            .pick_active_profile(Some("qwen-coder"), Some("nemotron"), "nemotron3:33b")
            .unwrap()
            .unwrap();
        assert_eq!(p.name, "qwen-coder");
        assert_eq!(p.via, PickVia::Profile);
        // 2. --bundle resolves to its profile for the model
        let p = cfg
            .pick_active_profile(None, Some("nemotron"), "nemotron3:33b")
            .unwrap()
            .unwrap();
        assert_eq!(
            (p.name.as_str(), p.via),
            ("nemotron", PickVia::Bundle("nemotron".into()))
        );
        // 3. inferred from the model when neither flag is set
        let p = cfg
            .pick_active_profile(None, None, "nemotron3:33b")
            .unwrap()
            .unwrap();
        assert_eq!(p.via, PickVia::InferredBundle("nemotron".into()));
        // 4. nothing matches → None (today's behavior)
        assert!(cfg
            .pick_active_profile(None, None, "gpt-4.1")
            .unwrap()
            .is_none());
        // an unknown explicit bundle is a hard error
        assert!(cfg.pick_active_profile(None, Some("ghost"), "x").is_err());
    }

    // ── loadouts (the top-level composition; inert until Slice 1) ───────

    #[test]
    fn loadout_parses_inline_and_validates_references() {
        let cfg: Config = toml::from_str(
            r#"
            [[backends]]
            name = "dgx"
            endpoint = "http://dgx.local:11434"
            model = "nemotron-3:33b"
            tiers = []

            [profiles.nemotron]
            techniques = ["knowledge_base", "verify_gate", "retry"]
            [bundles.nemotron]
            default_profile = "nemotron"

            [loadouts.dev-nemotron]
            provider = "dgx"
            model    = "nemotron@deep"
            kit      = "nemotron"
            profile  = "nemotron"
            role     = "python-developer"
            [loadouts.dev-nemotron.settings]
            num_ctx = 24576
            framing = "Ship small, verify."
            "#,
        )
        .unwrap();
        let l = &cfg.loadouts["dev-nemotron"];
        assert_eq!(l.provider.as_deref(), Some("dgx"));
        assert_eq!(l.model.as_deref(), Some("nemotron@deep"));
        assert_eq!(l.role.as_deref(), Some("python-developer"));
        assert_eq!(l.settings.as_ref().unwrap().num_ctx, Some(24576));
        // references resolve
        assert!(l.validate(&cfg).is_ok());
    }

    #[test]
    fn loadout_rejects_dangling_references() {
        let cfg: Config = toml::from_str(
            r#"
            [profiles.nemotron]
            techniques = ["verify_gate"]
            "#,
        )
        .unwrap();
        // dangling kit
        let bad_kit = Loadout {
            kit: Some("ghost-bundle".into()),
            ..Default::default()
        };
        let e = bad_kit.validate(&cfg).unwrap_err();
        assert!(
            e.contains("kit 'ghost-bundle'") && e.contains("no such bundle"),
            "{e}"
        );
        // dangling profile
        let bad_profile = Loadout {
            profile: Some("ghost-profile".into()),
            ..Default::default()
        };
        let e = bad_profile.validate(&cfg).unwrap_err();
        assert!(
            e.contains("profile 'ghost-profile'") && e.contains("no such profile"),
            "{e}"
        );
        // dangling provider — must name a [backends] entry (Slice 2). With no
        // `[[backends]]` in this TOML, `cfg.backends` is the default `ollama`.
        let bad_provider = Loadout {
            provider: Some("ghost-provider".into()),
            ..Default::default()
        };
        let e = bad_provider.validate(&cfg).unwrap_err();
        assert!(
            e.contains("provider 'ghost-provider'")
                && e.contains("no [backends] entry")
                && e.contains("ollama"),
            "{e}"
        );
        // an empty loadout is valid (no references)
        assert!(Loadout::default().validate(&cfg).is_ok());
    }

    #[test]
    fn disk_bundles_load_per_file_by_stem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("nemotron.toml"),
            "applies_to = [\"nemotron\"]\ndefault_profile = \"nemotron\"\n",
        )
        .unwrap();
        // a malformed drop-in must be skipped, not break loading
        std::fs::write(
            dir.path().join("broken.toml"),
            "applies_to = \"not-a-list\"\n",
        )
        .unwrap();
        // a non-toml file is ignored
        std::fs::write(dir.path().join("README.md"), "not a bundle").unwrap();

        let mut cfg = Config::default();
        cfg.merge_bundles_from_dir(dir.path());
        assert_eq!(cfg.bundles.len(), 1, "only the valid .toml loads");
        let b = cfg
            .bundles
            .get("nemotron")
            .expect("loaded by filename stem");
        assert_eq!(b.applies_to, vec!["nemotron"]);
        assert_eq!(b.default_profile.as_deref(), Some("nemotron"));
        // a disk file overrides an inline bundle of the same name (last-wins)
        cfg.bundles.insert("x".into(), BundleConfig::default());
        std::fs::write(dir.path().join("x.toml"), "about = \"from disk\"\n").unwrap();
        cfg.merge_bundles_from_dir(dir.path());
        assert_eq!(cfg.bundles["x"].about.as_deref(), Some("from disk"));
    }

    #[test]
    fn disk_loadouts_load_per_file_by_stem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("dev-nemotron.toml"),
            "provider = \"dgx\"\nmodel = \"nemotron@deep\"\nkit = \"nemotron\"\n",
        )
        .unwrap();
        // a malformed drop-in must be skipped, not break loading
        std::fs::write(
            dir.path().join("broken.toml"),
            "provider = [\"not-a-string\"]\n",
        )
        .unwrap();
        // a non-toml file is ignored
        std::fs::write(dir.path().join("README.md"), "not a loadout").unwrap();

        let mut cfg = Config::default();
        cfg.merge_loadouts_from_dir(dir.path());
        assert_eq!(cfg.loadouts.len(), 1, "only the valid .toml loads");
        let l = cfg
            .loadouts
            .get("dev-nemotron")
            .expect("loaded by filename stem");
        assert_eq!(l.provider.as_deref(), Some("dgx"));
        assert_eq!(l.model.as_deref(), Some("nemotron@deep"));
        assert_eq!(l.kit.as_deref(), Some("nemotron"));
        // a disk file overrides an inline loadout of the same name (last-wins)
        cfg.loadouts.insert("x".into(), Loadout::default());
        std::fs::write(dir.path().join("x.toml"), "role = \"from-disk\"\n").unwrap();
        cfg.merge_loadouts_from_dir(dir.path());
        assert_eq!(cfg.loadouts["x"].role.as_deref(), Some("from-disk"));
    }

    #[test]
    fn surface_match_round_trips_lowercase() {
        let k: VerifyGateKnobs = toml::from_str("surface_match = \"prefix\"").unwrap();
        assert_eq!(k.surface_match, crate::verify_gate::SurfaceMatch::Prefix);
    }

    #[test]
    fn resolve_profile_looks_up_validates_and_errors() {
        let cfg: Config = toml::from_str(
            r#"
            [profiles.nemotron]
            techniques = ["verify_gate"]
            [profiles.bad]
            techniques = ["teleport"]
            "#,
        )
        .unwrap();
        // known + valid → the profile
        assert!(cfg
            .resolve_profile("nemotron")
            .unwrap()
            .enables("verify_gate"));
        // known name but invalid technique → validation error
        assert!(cfg.resolve_profile("bad").unwrap_err().contains("teleport"));
        // unknown name → no-such-profile error, listing the known ones
        let err = cfg.resolve_profile("ghost").unwrap_err();
        assert!(
            err.contains("no such profile") && err.contains("nemotron"),
            "err: {err}"
        );
    }

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
    fn memory_extract_notes_on_close_defaults_off_and_parses() {
        // Default OFF (Step 19.4, #248): the close-time extraction pass is
        // optional and costs a completion — nobody pays for it unasked.
        assert!(!MemoryConfig::default().extract_notes_on_close);
        let cfg: MemoryConfig = toml::from_str("provider = \"rolling_window\"").unwrap();
        assert!(!cfg.extract_notes_on_close);
        // `[memory] extract_notes_on_close = true` is the opt-in.
        let cfg: MemoryConfig = toml::from_str("extract_notes_on_close = true").unwrap();
        assert!(cfg.extract_notes_on_close);
    }

    #[test]
    fn memory_disclosure_defaults_to_frozen_and_parses_index() {
        // INERT BY DEFAULT (#319): the disclosure facet defaults to Frozen —
        // today's behavior, the memory_fetch tool unwired — and only `index`
        // opts in to progressive disclosure.
        assert_eq!(MemoryConfig::default().disclosure, MemoryDisclosure::Frozen);
        let cfg: MemoryConfig = toml::from_str("provider = \"rolling_window\"").unwrap();
        assert_eq!(cfg.disclosure, MemoryDisclosure::Frozen);
        let cfg: MemoryConfig = toml::from_str("disclosure = \"index\"").unwrap();
        assert_eq!(cfg.disclosure, MemoryDisclosure::Index);
        let cfg: MemoryConfig = toml::from_str("disclosure = \"frozen\"").unwrap();
        assert_eq!(cfg.disclosure, MemoryDisclosure::Frozen);
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
        // 17.7: auto-resume defaults ON; `resume = false` is the off-switch.
        assert!(conversations.resume);
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

        let conversations = cfg.conversations.unwrap_or_default();
        assert_eq!(conversations.max_per_workspace, 25);
        // Partial [conversations] table: unset keys keep their defaults.
        assert!(conversations.resume);
    }

    #[test]
    fn conversations_resume_off_switch_parses() {
        let cfg: Config = toml::from_str(
            r#"
[conversations]
resume = false
"#,
        )
        .unwrap();

        assert!(!cfg.conversations.unwrap_or_default().resume);
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
model = "gpt-4.1-mini"
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
        assert_eq!(cfg.providers[0].model.as_deref(), Some("gpt-4.1-mini"));
        assert_eq!(cfg.providers[0].env_pass, vec!["CLOUD_TOKEN".to_string()]);
    }

    #[test]
    fn provider_model_is_optional_for_legacy_configs() {
        let cfg: Config = toml::from_str(
            r#"
[[providers]]
name = "legacy-cloud"
command = "newt-cloud-shim"
env_pass = ["CLOUD_TOKEN"]
tiers = ["COMPLEX"]
"#,
        )
        .unwrap();

        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].model, None);
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
    fn to_redacted_toml_hides_mcp_secrets_but_keeps_shape() {
        let cfg: Config = toml::from_str(
            r#"
            [[backends]]
            name = "remote"
            endpoint = "http://remote:8000"
            model = "qwen3:32b"
            tiers = []
            kind = "openai"
            api_key_file = "~/.newt/openai.key"

            [[mcp_servers]]
            name = "gh"
            type = "http"
            url = "https://api.example/mcp"
            [mcp_servers.headers]
            Authorization = "Bearer sk-super-secret-token"
            [mcp_servers.env]
            GH_TOKEN = "ghp_rawsecretvalue"
            RUST_LOG = "debug"
            "#,
        )
        .unwrap();

        let dump = cfg.to_redacted_toml().unwrap();
        // The raw secret VALUES never appear…
        assert!(
            !dump.contains("sk-super-secret-token"),
            "header secret leaked:\n{dump}"
        );
        assert!(
            !dump.contains("ghp_rawsecretvalue"),
            "env secret leaked:\n{dump}"
        );
        // …but the KEYS and the placeholder do, so the audit shows the shape.
        assert!(dump.contains("Authorization"));
        assert!(dump.contains("GH_TOKEN"));
        assert!(dump.contains(Config::REDACTED));
        // Secret *references* (a path) are kept — they name where a secret lives.
        assert!(
            dump.contains("~/.newt/openai.key"),
            "api_key_file reference kept"
        );
        // Non-secret structure is intact.
        assert!(dump.contains("http://remote:8000"));
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
            prompt: false,
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
            prompt: false,
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
            prompt: false,
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
            prompt: false,
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
            prompt: false,
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
            prompt: false,
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
            prompt: false,
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
            prompt: false,
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
            prompt: false,
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
