//! Configuration loading for Newt-Agent.
//!
//! Base resolution order: `$NEWT_CONFIG` env var, then `./newt.toml`,
//! `$NEWT_CONFIG_DIR/config.toml` (or `~/.newt/config.toml`), then
//! `/etc/newt/config.toml`. If none exist the built-in defaults are used
//! (a single Ollama backend on localhost).
//!
//! A project-local `.newt/config.toml` (found by walking up from the current
//! directory) is then deep-merged **over** that base, so a git repo can pin its
//! own models, endpoints, rules, and local stdio MCP services without copying
//! the whole global config. See [`Config::resolve`] and issue #222.

// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 18:33 EDT | Date: 2026-08-12

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{NewtError, Result};
use crate::router::Tier;
pub(crate) use backend::resolve_api_key_common;
pub use backend::{
    derive_serving, set_cli_backend_override, BackendConfig, BackendDestination, BackendKind,
    BackendOverride, BackendProvenance, BackendRef, BackendRequest, BackendResolutionReceipt,
    BackendSlots, DeclaredBackend, Engine, ManagedMode, OpenAiApi, RequestMode, Serving,
};
pub use context::{
    compaction_trigger_is_session_pinned, input_percentage_ceiling, normalize_input_ceiling_pct,
    session_compaction_trigger_policy, CompactionTriggerPolicy, ContextConfig, ContextFeature,
    ContextFeatureSet, ContextFeatures, ContextManager,
};
pub use crew::{Crew, CrewBudgets, CrewPolicyConfig};
pub use dropin::*;
pub(crate) use layering::{
    expand_tilde, find_ancestor_dir, find_project_config_from, home_dir, merge_project_overlay,
    merge_toml, strip_control_plane,
};
pub use layering::{ArrayMergeStrategy, MergeConfig};
pub use loadout::{Loadout, LoadoutSettings};
pub use newt_tuner::ModelTuning;
pub use permissions::{ModeConfig, PermissionPreset, ToolPermissions};
pub use presentation::{
    markdown_is_session_pinned, session_markdown_mode, session_spill_lines,
    set_session_spill_lines, ColorMode, EditMode, FooterMode, MarkdownMode, ThinkingMode,
};
pub use profile::{
    BundleConfig, PickVia, ProfileConfig, ProfilePick, RetryKnobs, VerifyGateKnobs,
    KNOWN_TECHNIQUES,
};
pub use tool_exposure::{ExposureProfile, ToolExposureConfig};
pub use tools::ToolsConfig;

mod backend;
mod context;
mod crew;
mod dropin;
mod layering;
mod loadout;
mod permissions;
mod presentation;
mod profile;
mod redact;
mod shell;
mod tool_exposure;
mod tools;
use backend::{cli_backend_override, validate_backend_names, BackendAssembly, RecordTag};
#[cfg(test)]
use context::default_input_ceiling_pct;
use layering::{array_merge_strategy, base_is_ambient_newt_toml, mark_project_mcp_untrusted};
use presentation::{default_spill_lines, default_time_marker_secs, default_tool_output_lines};
use redact::{redact_arg_secrets, redact_url_secrets};
pub use shell::{
    confined_default_engine, full_access_default_engine, mcp_stdio_env_passthrough,
    ocap_l3_backend, resolve_shell_engine, resolve_shell_engine_choice, resolved_confined_default,
    shell_env_passthrough_default, IntakeConfig, ShellConfig, ShellEngine,
};
use tools::{
    default_max_output_tokens, default_output_cap_chars_per_token, default_output_head_tokens,
};

/// Process-scoped user config root override, set by the CLI's `--config-dir`.
pub const NEWT_CONFIG_DIR_ENV: &str = "NEWT_CONFIG_DIR";

// ---------------------------------------------------------------------------
// Config types
// ---------------------------------------------------------------------------

/// `[scratch]` — the ephemeral-state location (#844). See [`Config::scratch`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScratchConfig {
    /// The scratch dir: relative (under the repo, default `.scratch`) or absolute
    /// (`/tmp`, a PVC mount) for a read-only checkout. `NEWT_SCRATCH_DIR` wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// Top-level Newt-Agent configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Inference backends (Ollama, vLLM, etc.).
    ///
    /// Absent `[[backends]]` deserializes to **empty** (not the struct-level
    /// default's localhost fallback) so a config that defines its backends as
    /// per-file `~/.newt/backends/*.toml` drop-ins does NOT also pick up a
    /// spurious synthesized `ollama` entry. The localhost fallback is restored
    /// in [`Config::resolve`] only if backends are still empty after the disk
    /// merge (so a truly bare setup still talks to a local Ollama).
    #[serde(default = "Vec::new")]
    pub backends: Vec<BackendConfig>,

    /// Provenance marker for [`Config::is_unconfigured`]: true while the
    /// backend list is exactly the compiled-in localhost fallback — nothing
    /// operator-supplied (no inline `[[backends]]`, no drop-in, no CLI
    /// override). Maintained deterministically by [`Config::resolve`] and
    /// `Default` (serde is never trusted for it: `#[serde(skip)]`, and
    /// `resolve()` recomputes it at every backend-assembly stage). Meaningful
    /// only on a `resolve()`d config; never serialized. `pub` only because
    /// `Config`'s struct-update syntax needs every field visible — read it
    /// through [`Config::is_unconfigured`], never directly.
    #[doc(hidden)]
    #[serde(skip)]
    pub backend_fallback: bool,

    /// Which backend the session starts on when several are configured and no
    /// env/loadout pins one (#1130, epic #1126). Unset + multiple backends =
    /// today's heuristic (prefer openai). Points at a backend NAME.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_backend: Option<String>,

    /// `[discovery]` — where `newt setup` looks for live inference endpoints
    /// (#1130). Defaults cover the localhost unboxing case; add hosts for a
    /// home-lab sweep. Pure data; only `setup`/`doctor` read it.
    #[serde(default)]
    pub discovery: Discovery,

    /// `[network]` — the operator's "these hosts are mine" declaration
    /// (#1789). Colloquial trust: it shapes retry patience, never authority.
    /// It cannot widen the note exfiltration guard; see [`crate::owned_hosts`].
    #[serde(default)]
    pub network: NetworkConfig,

    /// External provider-plugin definitions.
    pub providers: Vec<ProviderConfig>,

    /// `[scratch]` — where ephemeral state (crew worktrees, the crew cargo
    /// target, per-session plans) lives (#844). `dir` may be relative (under the
    /// repo, default `.scratch`) or absolute (`/tmp`, a k8s PVC mount) for
    /// read-only checkouts. `NEWT_SCRATCH_DIR` overrides it. Applied in
    /// [`Config::apply_runtime_settings`] via [`crate::scratch::set_scratch_dir`].
    #[serde(default)]
    pub scratch: Option<ScratchConfig>,

    /// Default tier ordering used by the router when no per-backend
    /// override is specified.
    pub default_tier_order: Vec<Tier>,

    /// `[lifecycle]` — the repo's build/dev commands per lifecycle phase
    /// (`format`, `check`, `clean`, …), #880. Overrides the per-ecosystem tooling
    /// packs. Applied in [`Config::resolve`] via
    /// [`crate::tooling::set_lifecycle_override`].
    #[serde(default)]
    pub lifecycle: Option<crate::tooling::PhaseCommands>,

    /// Optional NVIDIA DGX endpoint-management config powering the
    /// `newt dgx` command suite. `None` when unconfigured — newt never
    /// dials a DGX endpoint unless this (or a `NEWT_DGX_*` env var) is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dgx: Option<crate::dgx::DgxConfig>,

    /// TUI appearance and behaviour. `None` → built-in defaults apply.
    /// Overridable at runtime via `NEWT_CHAT_STYLE` and `NEWT_PROMPT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui: Option<TuiConfig>,

    /// `[shell]` — which engine runs `run_command` (ADR 0005 D2 seam). `None` /
    /// unset → the `safe-subset` default, except `--full-access` auto-upgrades to
    /// `host`. Overridable per-session by `--shell-engine`. The L3 backend
    /// (Landlock/Seatbelt/AppContainer) is a separate, auto-selected axis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellConfig>,

    /// `[intake]` — prompt-disposition inference overrides (#1260). `None` →
    /// the built-in [`crate::agentic::DispositionLexicon`] defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intake: Option<IntakeConfig>,

    /// `[context]` — context-management strategy selection (Step 24.8, #559).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextConfig>,

    /// `[tools]` — tool-execution behaviour (#726). `None` → built-in defaults
    /// (notably `max_output_tokens` = 10000). See [`ToolsConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsConfig>,

    /// `[tenacity]` — how hard the harness pushes the model from reading to
    /// acting: a baseline level plus per-model-family overrides. `None` → the
    /// behaviour-preserving `Standard`. An explicit `--tenacity` supersedes it.
    /// See [`crate::tenacity::TenacityConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenacity: Option<crate::tenacity::TenacityConfig>,

    /// `[tool_exposure]` — the progressive tool-schema controller (Pass 1).
    /// `None` → [`ExposureProfile::Full`] (identity; advertise the full
    /// authorized catalog). See [`ToolExposureConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_exposure: Option<ToolExposureConfig>,

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
    /// [`crate::NamedPermissionPreset`]) and, when applied via `/posture`, clamps
    /// the session's authority as a hard floor. Empty by default — no preset,
    /// behavior unchanged.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub permission_presets: std::collections::BTreeMap<String, crate::NamedPermissionPreset>,

    /// Named permission-posture bindings for `/posture` (issue #307). The
    /// `[modes.<name>]` key is retained for configuration compatibility. Each
    /// binding atomically preloads a skill body, applies a permission preset as
    /// an authority floor, and adds system-prompt framing. Empty by default.
    /// See [`ModeConfig`].
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

    /// Named crews (`[crews.<name>]` or `crews/<name>.toml`) — role-specialized
    /// ensembles over the backend pool (`docs/design/crew-loadout.md`). Each role
    /// names a `[loadouts.<name>]`. Empty by default. See [`Crew`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub crews: std::collections::BTreeMap<String, Crew>,

    /// `[crew]` — crew/team **dispatch policy** (#749). Carries the authority
    /// *clamp* every dispatched crew is met against, so a crew's effective
    /// authority is `session ⊓ clamp` — never above the session ceiling, and as
    /// tight as the operator configures. `None` (and the default clamp) is
    /// `Caveats::top()`, i.e. the meet is the identity and behavior is unchanged.
    /// This is the structural tightening point the per-subtask `team_clamp`
    /// (#749 step 8) plugs into. See [`CrewPolicyConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crew: Option<CrewPolicyConfig>,

    /// `[plan]` — plan-authoring policy. Today: the `[plan.prune]` droppable
    /// override for the decompose prune's anti-pattern lexicon (#801/#803 →
    /// #819). `None` = compiled defaults, behavior unchanged. See
    /// [`PlanPruneConfig`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanConfig>,
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
    /// session start. "Most recently active" means the highest §6 activity
    /// tick — never a wall-clock comparison.
    ///
    /// **Default: false** (#1030). Each launch starts a FRESH conversation, so
    /// running `newt` several times in one folder no longer drags every session
    /// into — and interleaves their turns onto — that folder's single latest
    /// conversation (the collision the old `true` default caused). Find and
    /// reopen a past conversation explicitly with `/resume` instead. Opt back
    /// into the old auto-resume-latest behavior with:
    ///
    /// ```toml
    /// [conversations]
    /// resume = true       # auto-resume the folder's most recent conversation
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
    // #1030: fresh-on-launch. The old `true` made every launch in a folder
    // auto-resume that folder's latest conversation, so concurrent `newt`
    // processes interleaved their turns into one record. `/resume` is now the
    // explicit way back into a past conversation.
    false
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

    /// Directory of bundled skills shipped with newt-agent. Scanned *after* the
    /// user's `search` paths — i.e. at the **lowest** priority — so a user skill
    /// of the same name shadows the bundled one (earlier directories win a
    /// collision; see [`newt_skills::discover_paths`]). Empty → no bundled
    /// directory is scanned. `~/` is expanded to `$HOME`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bundled_dir: String,
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
    // INERT-CODE-RATCHET: F04 WIRE: max_age_days is parsed and defaulted but no retention decision reads it.
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

/// One symbol-extraction rule in a [`LanguagePack`]: a regex over a single source
/// line whose **first capture group is the public symbol's name**, plus a
/// free-form kind label. Free-form so a pack is not locked to one language's
/// vocabulary (`fn`/`struct` for Rust, `class`/`def` for Python, `func` for Go…).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRule {
    /// Regex; capture group 1 = the symbol name.
    pub pattern: String,
    /// Kind shown in the surface (e.g. `"fn"`, `"struct"`, `"class"`, `"func"`).
    pub kind: String,
}

/// A **language pack** for the workspace API surface (#669): how to recognize a
/// language's files, which files expose its public API, and how to extract its
/// public symbols — entirely as DATA, so a new language is config, not code.
///
/// Built-in packs cover common source languages. A project ships more by
/// dropping a `<name>.toml` into `~/.newt/language-packs/` (global) or
/// `.newt/language-packs/` (project-local), or inline under
/// `[[context.api_surface.language_packs]]`. Packs merge **by `name`** (a custom
/// pack with a built-in's name replaces it), so anyone can add Java, Ruby, Swift,
/// Objective-C, … without touching the binary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguagePack {
    /// Stable id (a config pack with a built-in's name replaces that built-in).
    pub name: String,
    /// Human spellings accepted by harness source-file classification, e.g.
    /// `["c++", "cpp"]` or `["c#", "dotnet"]`. The stable `name` is always an
    /// implicit alias. Pure data keeps language understanding out of prompt-
    /// specific conditionals.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// File extensions this pack claims, no dot — `["rs"]`, `["h", "hpp", "cpp"]`.
    pub extensions: Vec<String>,
    /// Entry-point filename globs (the public-API files, listed first in the
    /// surface). Supported globs: exact (`lib.rs`), suffix (`*.h`), or all (`*`).
    /// Empty ⇒ no file is prioritized for this pack.
    #[serde(default)]
    pub entry_points: Vec<String>,
    /// Public-symbol extraction rules, applied per source line.
    pub symbols: Vec<SymbolRule>,
}

/// `[context.api_surface]` — the workspace-API-surface knowledge_base technique.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiSurfaceConfig {
    /// Inline language packs, merged by `name` over the built-ins and the
    /// drop-in directories (the highest-precedence layer).
    #[serde(default)]
    pub language_packs: Vec<LanguagePack>,
    /// **Deprecated** operator pin. When present, the tier-2 budget is pinned to
    /// this char count (`floor_chars == ceiling_chars == max_block_chars`) — the
    /// legacy fixed cap. Prefer the proportional trio below (spec §3, SC-L2).
    /// Absent by default so the surface scales with the discovered window.
    #[serde(default)]
    pub max_block_chars: Option<usize>,
    /// SC-L2 floor: the minimum tier-2 char allowance, even on a tiny window —
    /// the surface must never be starved to nothing (dominates near ~8k tokens).
    #[serde(default = "default_api_surface_floor_chars")]
    pub floor_chars: usize,
    /// SC-L2 slope: percent of the resolved send budget `w` (tokens) the tier-2
    /// surface may claim, before the chars/token conversion and clamp.
    #[serde(default = "default_api_surface_pct_of_budget")]
    pub pct_of_budget: usize,
    /// SC-L2 ceiling — a §8 *pin*, not law: the max tier-2 char allowance on a
    /// large window; the v1 value is set empirically by the #548 map-size arms.
    #[serde(default = "default_api_surface_ceiling_chars")]
    pub ceiling_chars: usize,
    /// Per-file symbol cap, so one huge file can't crowd out the surface.
    #[serde(default = "default_api_surface_max_symbols_per_file")]
    pub max_symbols_per_file: usize,
}

impl Default for ApiSurfaceConfig {
    fn default() -> Self {
        Self {
            language_packs: Vec::new(),
            max_block_chars: None,
            floor_chars: default_api_surface_floor_chars(),
            pct_of_budget: default_api_surface_pct_of_budget(),
            ceiling_chars: default_api_surface_ceiling_chars(),
            max_symbols_per_file: default_api_surface_max_symbols_per_file(),
        }
    }
}

// SC-L2 pins (spec §8). Defaults chosen so the floor dominates at the
// DEFAULT_CONTEXT_TOKENS=8,192 fallback (8192·5% ·4 = 1,638 < 2,000) and the
// ceiling caps a 262k-window session (its ~168k send budget · 5% · 4 ≫ 24,000).
fn default_api_surface_floor_chars() -> usize {
    2_000
}

fn default_api_surface_pct_of_budget() -> usize {
    5
}

fn default_api_surface_ceiling_chars() -> usize {
    24_000
}

fn default_api_surface_max_symbols_per_file() -> usize {
    12
}

/// `[context.semantic]` — the embedding RAG-for-code feature's settings (Step
/// 26.5.4, #582).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticConfig {
    /// Embedding model used to index the repo + embed queries. Default
    /// `nomic-embed-text` (the HTTP path). The model must exist on the embeddings
    /// endpoint (see `embeddings_endpoint`); when it can't be reached the feature
    /// follows `on_embed_failure`.
    ///
    /// For the **embedded backend** (`embeddings_api = "embedded"`, #720) this is
    /// only a label — the model is loaded from `embedding_model_path` — and it
    /// should name a **candle-clean standard-BERT** model (e.g.
    /// `bge-small-en-v1.5`), NOT `nomic-embed-text`, which candle 0.8 cannot load.
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    /// Local model **directory** for the embedded embedder (#720): a
    /// candle-clean standard-BERT model dir holding
    /// `config.json` + `tokenizer.json` + `model.safetensors` (e.g. a fetched
    /// `BAAI/bge-small-en-v1.5`). `None` (default) ⇒ the embedded path can't
    /// load and reports a clear error. When `embeddings_api` and
    /// `embeddings_endpoint` are unset, a configured path selects embedded
    /// embeddings automatically. Ignored by explicit HTTP embeddings targets.
    /// Mirrors the summarizer's `model_path`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model_path: Option<String>,
    /// How many code chunks to retrieve per turn. Default 5.
    #[serde(default = "default_semantic_top_k")]
    pub top_k: usize,
    /// Dedicated endpoint that serves embeddings (e.g. an Ollama
    /// `http://host:11434`). `None` (default) leaves semantic retrieval on the
    /// embedded path unless `embeddings_api` explicitly selects an HTTP protocol.
    /// Set this to a real embeddings host when remote/vector-server embeddings
    /// are a deliberate performance choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddings_endpoint: Option<String>,
    /// Wire protocol of `embeddings_endpoint` — `ollama` (`/api/embeddings`) or
    /// `openai` (`/v1/embeddings`). `embedded` selects the in-process embedder.
    /// `None` (default) selects embedded embeddings when `embeddings_endpoint`
    /// is also unset; with an explicit endpoint, `None` assumes `ollama`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddings_api: Option<BackendKind>,
    /// What to do when embedding fails structurally (wrong endpoint / model
    /// absent): `disable` (default) stops indexing after the first failure with
    /// one actionable message; `warn` logs per-chunk and keeps trying.
    #[serde(default)]
    pub on_embed_failure: OnEmbedFailure,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            embedding_model: default_embedding_model(),
            embedding_model_path: None,
            top_k: default_semantic_top_k(),
            embeddings_endpoint: None,
            embeddings_api: None,
            on_embed_failure: OnEmbedFailure::default(),
        }
    }
}

/// Policy when an embedding request fails structurally during indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OnEmbedFailure {
    /// Stop indexing on the first failure and log one actionable error — a
    /// structural failure (wrong endpoint / missing model) is total, not
    /// transient, so degrading per-chunk just produces an empty index quietly.
    #[default]
    Disable,
    /// Log every failed chunk and keep going (the historical behaviour).
    Warn,
}

fn default_embedding_model() -> String {
    "nomic-embed-text".to_string()
}

fn default_semantic_top_k() -> usize {
    5
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

    /// Opt into the mouse-driven spill-viewport tier (#1303): wheel/click scroll
    /// and editor-mode-aware keys on the live-spill viewport. Default **off**; it
    /// only ever activates on top of `live_spill_capable()` on an interactive
    /// stdin+stdout TTY (a strict superset gate), and the mouse code is
    /// compile-time-stripped from the lean/wyvern build.
    #[serde(default)]
    pub mouse_viewport: bool,

    /// Key binding mode for the chat input line.
    /// `"emacs"` (default), `"vi"`, or `"nano"`. Also overridable via
    /// `NEWT_EDIT_MODE`. (`nano` is emacs-style/modeless — it differs from
    /// `emacs` only in label today; the rich-tui surface honors it.)
    #[serde(default)]
    pub edit_mode: EditMode,

    /// Rich-tui (issue #416) input gutter width, in columns. Unset = `auto`
    /// (the responsive default: a prompt gutter when it fits under ~1/3 of the
    /// width, else a stacked prompt row). `0` turns the gutter off (prompt on
    /// its own row, input flush-left); a positive value indents the input that
    /// many columns (a value wide enough to hold the prompt renders it inline).
    /// No effect on the lean surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gutter: Option<u16>,

    /// Input-footer mode: the transient multi-line `❯` input block with a
    /// status header. `"auto"` (default) shows it on a TTY and degrades to a
    /// plain scroller otherwise; `"on"` always shows it; `"off"` never does
    /// (the `--plain` CLI flag, or `NEWT_FOOTER=off`).
    #[serde(default)]
    pub footer: FooterMode,

    /// Color / theme mode (issue #527): `"auto"` (default), `"always"`,
    /// `"never"`, `"minimal"`, `"inverted"`, `"dark"`, `"light"`, `"mono"`.
    /// The `--color` CLI flag and `--mono` override this; `NO_COLOR` /
    /// `TERM=dumb` force it off unless an explicit `--color` is given. This is
    /// the on/off + theme *mode*; `[tui.colors]` below is the palette.
    #[serde(default)]
    pub color: ColorMode,

    /// How a thinking model's streamed reasoning is shown: `"stream"` (default
    /// — dim reasoning + a cargo-style spinner, TTY only) or `"off"`.
    #[serde(default)]
    pub thinking: ThinkingMode,

    /// Legacy line limit retained for pre-execution previews such as
    /// `write_file`. Completed tool results use `[tui].spill_lines` instead.
    /// Default: 20. Set to 0 to show the full preview.
    #[serde(default = "default_tool_output_lines")]
    pub tool_output_lines: usize,

    /// #1235: collapsed height of every tool's SPILL VIEW — the tail-biased,
    /// gutter-glyphed result block following its `⚙` audit line. It bounds both
    /// the live viewport and canonical completed result. An active TTY viewport
    /// may expand retained output up to safe terminal capacity. Default: 3.
    /// Set to 0 for unbounded completed output and no live viewport. The legacy
    /// `tool_output_lines` preview setting does not override it.
    #[serde(default = "default_spill_lines")]
    pub spill_lines: usize,

    /// Seconds between dim `[HH:MM]` markers committed above tool calls, so a
    /// long transcript can be read for WHEN as well as what. At most one marker
    /// per interval however long the gap — a turn that blocks for an hour emits
    /// one line when it returns, not twelve.
    ///
    /// Default: 300 (five minutes), and it applies to the INTERACTIVE surface
    /// only. Piped, headless and `newt solve` runs commit no markers whatever
    /// this says, because a wall clock in stdout makes byte-exact capture
    /// unstable and those are the paths where that matters. Set to 0 to turn
    /// them off interactively too.
    #[serde(default = "default_time_marker_secs")]
    pub time_marker_secs: u64,

    /// Maximum number of tool-call rounds the model may take within a single
    /// turn before the agent forces a final, tools-disabled completion. Each
    /// round is one model response that may emit tool calls; once this many
    /// rounds have run without a tool-free answer, newt asks the model once
    /// more with tools disabled so the user still gets a real (partial)
    /// answer instead of a placeholder. Default: 40 (raised from 25 — a
    /// modest safety margin alongside `workflow_grace_rounds` and the
    /// workflow-classifier delegate hint; genuinely open-ended diagnostic work
    /// should reach for `crew`/`team` delegation rather than depend on an
    /// unbounded cap here).
    #[serde(default = "default_max_tool_rounds")]
    pub max_tool_rounds: usize,

    /// Additional progress-aware rounds available after `max_tool_rounds` when
    /// an active workflow still has incomplete steps and the recent rounds show
    /// repair progress or actionable evidence. Default: 5. Set to 0 to make the
    /// normal round cap hard again.
    #[serde(default = "default_workflow_grace_rounds")]
    pub workflow_grace_rounds: usize,

    /// Maximum "you narrated intent but called no tool" auto-continue nudges
    /// per turn — the narrate-then-stop rescue. Once spent, the next no-tool
    /// narration is accepted as the turn's final answer and the turn ends.
    /// Default: 1. Weak local models that chronically announce actions in
    /// prose instead of calling tools benefit from 2–3; the second and later
    /// nudges escalate (they name the active plan step and demand a bare tool
    /// call). `0` disables the rescue entirely — every no-tool narration is
    /// accepted as the final answer. See docs/design/next-loop-levers.md,
    /// lever L3.
    #[serde(default = "default_narration_nudge_cap")]
    pub narration_nudge_cap: usize,

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
    /// mid-session. `None` → newt trusts the model's declared context window
    /// from Ollama `/api/show` and sends ~80 % of it as `num_ctx` (see
    /// `real_context_discovery`). Set an explicit cap here (e.g. 8192 / 16384)
    /// on VRAM-constrained hosts; this always takes precedence over the
    /// declared window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,

    /// Opt into **empirical** context-window discovery (the conserve-for-tiny-
    /// hardware mode). When `false`/unset (the default), newt **trusts the
    /// declared** `/api/show` window: `safe_context` tracks ~80 % of it and is
    /// raised back to it each session, so a model is never permanently capped
    /// by a past overflow. When `true`, newt instead keeps the conservative
    /// behaviour — bootstrap `safe_context` once, never auto-raise it, and let
    /// runtime context-window 400s ratchet it down and persist (for hardware
    /// that genuinely can't serve the full declared window). Overridable
    /// per-model via `[[model_tuning]] real_context_discovery`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real_context_discovery: Option<bool>,

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

    // Summarizer knobs (timeout / retries / fallback model) moved to the
    // dedicated `~/.newt/summarizer.toml` ([`SummarizerConfig`]) in Step 24.10
    // (#559), so the summarizer can run on its own backend. Old `[tui]` keys
    // (`summarizer_timeout_secs` / `summarizer_retries` / `summarizer_model`)
    // are no longer read — `#[serde(default)]` ignores them in stale configs.
    /// Markdown rendering of RichTUI text output (Step 25.4, #568), including
    /// assistant replies and built-in documents such as `/help`. `auto`
    /// (default) renders whenever color is active; `on`/`off` force it. The
    /// `/markdown [on|off]` command overrides this for the session.
    #[serde(default)]
    pub markdown: MarkdownMode,

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

    /// Whether the interactive `!` bang-escape is available — typing `! <cmd>`
    /// at the prompt runs a host command with the user's own authority (not the
    /// agent's OCAP leash). Default: `true`. Set `false` for locked-down or
    /// shared deployments where the human at the keyboard should not have an
    /// unconfined host shell-out. The model can never invoke `!` either way;
    /// this only governs the human. See `docs/decisions/plain_scroller_tui.md`.
    #[serde(default = "default_allow_bang_escape")]
    pub allow_bang_escape: bool,
    /// `tui-shell-commands`: human-typed navigation/inspection commands
    /// (`cd`/`pwd`/`ls`/`env`/`date`) handled by the TUI itself, managing a
    /// session working directory shown in the prompt — distinct from the agent's
    /// `run_command`. Default `true`. The model never sees these; this governs
    /// only the human at the keyboard.
    #[serde(default = "default_allow_shell_commands")]
    // INERT-CODE-RATCHET: F06 WIRE: shell admission and mutation knobs are parsed but neither shell path consults them.
    pub allow_shell_commands: bool,
    /// Whether the `tui-shell-commands` suite may MUTATE the filesystem
    /// (`mkdir`/`mv`, and `rm` via a recoverable graveyard). Default `false` —
    /// navigation + inspection only until the operator opts in.
    #[serde(default = "default_allow_shell_mutations")]
    pub allow_shell_mutations: bool,
}

fn default_max_tool_rounds() -> usize {
    40
}

fn default_workflow_grace_rounds() -> usize {
    5
}

fn default_narration_nudge_cap() -> usize {
    1
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

fn default_summarizer_timeout_secs() -> u64 {
    60
}

fn default_summarizer_retries() -> u32 {
    1
}

fn default_mid_loop_trim_threshold() -> usize {
    40
}

fn default_sanitize_mcp_server_names() -> bool {
    true
}

fn default_allow_bang_escape() -> bool {
    true
}

fn default_allow_shell_commands() -> bool {
    true
}

fn default_allow_shell_mutations() -> bool {
    false
}

// ---------------------------------------------------------------------------
// Per-model tuning helpers
// ---------------------------------------------------------------------------

impl Config {
    /// Find the first `[[model_tuning]]` entry whose `model` field matches
    /// `name` exactly. Returns `None` when no entry exists.
    #[must_use]
    pub fn find_model_tuning(&self, name: &str) -> Option<&ModelTuning> {
        self.model_tuning.iter().find(|t| t.model == name)
    }
}

/// `[plan]` — plan-authoring policy (#819).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanConfig {
    /// `[plan.prune]` — the decompose-prune lexicon override.
    #[serde(default)]
    pub prune: PlanPruneConfig,
}

/// `[plan.prune]` — droppable, three-Cs override for the #801/#803 planner-
/// decomposition prune. The compiled `ACTION_MARKERS` lexicon is the default;
/// this table composes with it (remove first, then additions), so a new
/// anti-pattern — or un-marking a verb your domain uses for real work — is
/// CONFIG, not code. The prune itself stays grade-neutral either way: it only
/// removes no-diff leaves before any authority grant (#803's n=5 A/B measured
/// no grade lift; it removes a failure *mechanism*).
///
/// ```toml
/// [plan.prune]
/// disabled = false
/// add_inspect = ["scrutinize"]   # pruned wherever they lead an instruction
/// add_gate = ["smoke"]           # pruned only when terminal
/// remove = ["review"]            # e.g. a repo where "Review X" edits docs
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PlanPruneConfig {
    /// Turn the prune off entirely (the pre-#803 behavior).
    #[serde(default)]
    pub disabled: bool,
    /// Extra leading verbs classified Inspect (case-insensitive).
    #[serde(default)]
    pub add_inspect: Vec<String>,
    /// Extra leading verbs classified Gate (case-insensitive).
    #[serde(default)]
    pub add_gate: Vec<String>,
    /// Verbs to REMOVE from the effective lexicon (builtin or added).
    #[serde(default)]
    pub remove: Vec<String>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            prompt: None,
            no_splash: false,
            mouse_viewport: false,
            edit_mode: EditMode::Nano,
            gutter: None,
            footer: FooterMode::Auto,
            color: ColorMode::Auto,
            thinking: ThinkingMode::Stream,
            tool_output_lines: default_tool_output_lines(),
            spill_lines: default_spill_lines(),
            time_marker_secs: default_time_marker_secs(),
            max_tool_rounds: default_max_tool_rounds(),
            workflow_grace_rounds: default_workflow_grace_rounds(),
            narration_nudge_cap: default_narration_nudge_cap(),
            permissions: ToolPermissions::default(),
            debug: None,
            trace: None,
            build_check_cmd: None,
            num_ctx: None,
            real_context_discovery: None,
            connect_timeout_secs: default_connect_timeout_secs(),
            inference_timeout_secs: default_inference_timeout_secs(),
            keep_alive: default_keep_alive(),
            markdown: MarkdownMode::default(),
            mid_loop_trim_threshold: default_mid_loop_trim_threshold(),
            mid_loop_trim_tokens: None,
            sanitize_mcp_server_names: default_sanitize_mcp_server_names(),
            mcp_allow_insecure_hosts: Vec::new(),
            allow_bang_escape: default_allow_bang_escape(),
            allow_shell_commands: default_allow_shell_commands(),
            allow_shell_mutations: default_allow_shell_mutations(),
        }
    }
}

/// Endpoint-discovery configuration (`[discovery]`, #1130): the hosts and
/// ports `newt setup` probes for live inference servers. Each host is tried
/// on every listed Ollama port (`/api/tags`) and vLLM/OpenAI port
/// (`/v1/models`); each hit becomes a `~/.newt/backends/<name>.toml`. Pure
/// data — the three Cs: adding a lab host is config, not code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Discovery {
    /// Hostnames to probe. Default: just localhost (the unboxing case).
    pub hosts: Vec<String>,
    /// Ports probed with the Ollama protocol (`/api/tags`).
    pub ollama_ports: Vec<u16>,
    /// Ports probed as OpenAI-compatible (`/v1/models`) — the vLLM range;
    /// several ports = several single-model instances (space multiplexing).
    pub vllm_ports: Vec<u16>,
}

/// `[network]` — which DNS suffixes the operator calls theirs.
///
/// Pure data, Configuration over hardcoded knowledge: the built-in private
/// suffixes stay compiled in as the floor, and this only ever *adds*. Declaring
/// `owned_suffixes = [".corp"]` makes `infer.corp` eligible for the patient
/// local-inference retry policy. It grants no authority and does not touch the
/// hardcoded exfiltration guard.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    /// DNS suffixes the operator owns (`.corp`, `.home.arpa`). A leading dot is
    /// optional; matching is case-insensitive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owned_suffixes: Vec<String>,
}

impl Default for Discovery {
    fn default() -> Self {
        Self {
            hosts: vec!["localhost".into()],
            ollama_ports: vec![11434],
            // vLLM convention first, then llama.cpp router mode; the remaining
            // adjacent ports cover operators hosting one vLLM instance per model.
            vllm_ports: vec![8000, 8080, 8001, 8002, 8003],
        }
    }
}

/// Shared provider-identity validation: nonempty and unique names. The
/// unified selection contract ([`Config::select_backend`]) name-addresses
/// providers, so a duplicate can hand the session a different provider
/// than the one the operator meant. Cross-namespace ties (a backend and a
/// provider sharing one name) are DELIBERATE and documented: a ROUTABLE
/// backend wins the tie; a destination-less backend loses it to the
/// provider — pinned by tests, not rejected.
fn validate_provider_names(providers: &[ProviderConfig]) -> std::result::Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for (i, p) in providers.iter().enumerate() {
        if p.name.trim().is_empty() {
            return Err(format!(
                "provider #{} has no name — every [[providers]] entry needs a unique \
                 `name` (selection is name-based)",
                i + 1
            ));
        }
        if !seen.insert(p.name.clone()) {
            return Err(format!(
                "two providers share the name `{}` — provider selection is name-based \
                 ($NEWT_PROVIDER, default_backend), so a duplicate can run the wrong \
                 one; rename one",
                p.name
            ));
        }
    }
    Ok(())
}

/// A backend with a COHERENT wire destination — a nonempty HTTP `endpoint`,
/// or a nonempty embedded `model_path` on a backend whose kind is embedded
/// (or unset — composition pins it to Embedded). The ONE usability
/// predicate selection runs on: an embedded backend is exactly as
/// selectable as an HTTP one, but a `model_path` on an HTTP-kind backend is
/// an incoherent declaration, not a route.
fn backend_is_routable(b: &BackendConfig) -> bool {
    if !b.endpoint.is_empty() {
        return true;
    }
    b.model_path.as_deref().is_some_and(|p| !p.is_empty())
        && matches!(b.kind, None | Some(BackendKind::Embedded))
}

/// The typed outcome of the shared slot selector — kept distinct so an
/// explicit selector naming a DESTINATION-LESS backend cannot collapse into
/// either "selected" (routing nowhere) or "nothing selected" (silently
/// running something else).
enum SlotSelection {
    /// The precedence picked this slot (routable by construction).
    Slot(usize),
    /// An explicit selector (`$NEWT_PROVIDER` / `default_backend`) named a
    /// configured backend with neither endpoint nor model_path. Surfaced,
    /// never silently skipped: consumers turn this into a hard error
    /// ([`SelectionOutcome::UnroutableNamed`], the assembly's field-only
    /// targeting error) or a documented `None`
    /// ([`Config::select_configured_backend`]).
    ExplicitlyUnroutable { name: String },
    /// An explicit selector names NOTHING among these backends — a typo, or
    /// a provider's name (this selector only knows `[[backends]]`; the
    /// caller that knows providers decides). Surfaced the same way:
    /// selection must not silently desert an explicit selector for the
    /// preference rules.
    ExplicitlyUnmatched { name: String },
    /// Nothing explicitly selected and nothing routable configured.
    None,
}

/// The ONE config-backend slot selector — shared by
/// [`Config::select_configured_backend`], [`Config::select_backend`],
/// [`ResolvedConfig::selected_backend`], and the assembly's unnamed
/// field-only CLI targeting, so "which backend is selected" has exactly one
/// answer. Precedence, most-specific first: `$NEWT_PROVIDER` names a
/// backend > `default_backend` > a sole routable backend > prefer an
/// OpenAI-kind routable entry, else the first routable one. An EXPLICIT
/// selector (env/default) naming a configured but destination-less backend
/// is [`SlotSelection::ExplicitlyUnroutable`] — never a silent pick of it,
/// never a silent fall-through past it.
fn select_backend_slot(
    backends: &[&BackendConfig],
    default_backend: Option<&str>,
) -> SlotSelection {
    // 1. Operator / live override: $NEWT_PROVIDER names a backend.
    if let Ok(name) = std::env::var("NEWT_PROVIDER") {
        if !name.is_empty() {
            return match backends.iter().position(|b| b.name == name) {
                Some(i) if backend_is_routable(backends[i]) => SlotSelection::Slot(i),
                Some(_) => SlotSelection::ExplicitlyUnroutable { name },
                None => SlotSelection::ExplicitlyUnmatched { name },
            };
        }
    }
    // 2. The configured default. An EMPTY string is absent, exactly as
    //    `select_backend` treats it — never an authoritative selector for a
    //    backend named "".
    if let Some(name) = default_backend.filter(|n| !n.is_empty()) {
        return match backends.iter().position(|b| b.name == name) {
            Some(i) if backend_is_routable(backends[i]) => SlotSelection::Slot(i),
            Some(_) => SlotSelection::ExplicitlyUnroutable {
                name: name.to_string(),
            },
            None => SlotSelection::ExplicitlyUnmatched {
                name: name.to_string(),
            },
        };
    }
    // 3. A sole backend is the obvious choice.
    if backends.len() == 1 {
        return match backends.first().filter(|b| backend_is_routable(b)) {
            Some(_) => SlotSelection::Slot(0),
            None => SlotSelection::None,
        };
    }
    // 4. Prefer an OpenAI-kind entry, else the first routable one.
    backends
        .iter()
        .position(|b| b.kind == Some(BackendKind::Openai) && backend_is_routable(b))
        .or_else(|| backends.iter().position(|b| backend_is_routable(b)))
        .map_or(SlotSelection::None, SlotSelection::Slot)
}

/// Selection follows the request in EVERY composer: a destination request
/// or a NAMED field-only edit makes its target the config-level selection
/// (`default_backend`), so an exclusive request can never leave a stale
/// default naming a discarded backend — with no CLI-installed
/// `$NEWT_PROVIDER`, typed/receipt selection would otherwise resolve
/// Unknown/None against a config that plainly contains the requested
/// backend. An UNNAMED field-only edit already targeted the selected
/// backend — nothing to move.
fn pin_requested_selection(
    cfg: &mut Config,
    over: Option<&BackendOverride>,
    requested_slot: Option<usize>,
) {
    let Some(over) = over.filter(|o| !o.is_empty()) else {
        return;
    };
    if over.endpoint.is_some() || over.model_path.is_some() || over.name.is_some() {
        if let Some(target) = requested_slot.and_then(|i| cfg.backends.get(i)) {
            cfg.default_backend = Some(target.name.clone());
        }
    }
}

/// A runtime-resolved configuration: the flattened [`Config`] plus the
/// per-backend provenance receipts, aligned **1:1 by slot** with
/// `config.backends` — receipt `i` is about backend `i`, full stop. Never
/// looked up by name or by `&BackendConfig` (both were how a receipt could
/// end up describing the wrong backend). Immutably derefs to [`Config`];
/// there is deliberately NO `DerefMut` — mutating the config would silently
/// invalidate the receipts.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    config: Config,
    receipts: Vec<BackendResolutionReceipt>,
}

/// One backend of a [`ResolvedConfig`]: the slot index, the effective
/// backend, and its provenance receipt — the three always travel together.
#[derive(Debug, Clone, Copy)]
pub struct ResolvedBackend<'a> {
    /// The slot index into `config.backends` / the receipts.
    pub slot: usize,
    /// The effective (flattened) backend at that slot.
    pub backend: &'a BackendConfig,
    /// The provenance receipt for that slot.
    pub receipt: &'a BackendResolutionReceipt,
}

impl std::ops::Deref for ResolvedConfig {
    type Target = Config;
    fn deref(&self) -> &Config {
        &self.config
    }
}

impl ResolvedConfig {
    /// Discard the receipts and keep the flattened config — the
    /// compatibility exit for consumers that predate receipts.
    #[must_use]
    pub fn into_config(self) -> Config {
        self.config
    }

    /// A resolution of `config` AS-IS: pure declarations — no disk merge,
    /// no CLI request, receipts minted 1:1 from each backend's own
    /// declaration. The INFALLIBLE last-resort constructor for surfaces
    /// that must render something even when resolution proper failed (the
    /// TUI's `unwrap_or_default` lane); it validates nothing, exactly like
    /// the bare `Config` it wraps.
    #[must_use]
    pub fn unrequested(config: Config) -> Self {
        let receipts = config
            .backends
            .iter()
            .map(|b| BackendResolutionReceipt {
                declaration: DeclaredBackend::of(b),
                request: None,
                observation: None,
                binding: crate::model_card::CardBindingSeed::from_backend(b),
            })
            .collect();
        Self { config, receipts }
    }

    /// Test-support (doc-hidden, the `test_guard` precedent): run the
    /// backend assembly over `config` + explicit drop-in `dirs` + an
    /// explicit request, exactly as `resolve_runtime_unpublished` does but
    /// without candidate-path/env IO — so dependent crates' tests can
    /// build receipt-bearing resolutions deterministically.
    ///
    /// # Errors
    /// The assembly's identity/destination/request errors.
    #[doc(hidden)]
    pub fn assemble_for_test(
        mut config: Config,
        dirs: &[&Path],
        over: Option<BackendOverride>,
    ) -> std::result::Result<Self, String> {
        let mut assembly = BackendAssembly::new(std::mem::take(&mut config.backends))?;
        for dir in dirs {
            assembly.merge_dir(dir)?;
        }
        let default_backend = config.default_backend.clone();
        let _slot = assembly.apply_request(over, default_backend.as_deref())?;
        let (backends, receipts) = assembly.finish();
        config.backends = backends;
        Ok(Self { config, receipts })
    }

    /// Publish this resolution's process-global settings —
    /// [`Config::publish_runtime_settings`] without giving up the receipts:
    /// validate first, publish explicitly, keep selecting through the
    /// receipt-bearing view.
    pub fn publish_runtime_settings(&self) {
        self.config.publish_runtime_settings();
    }

    /// The receipts, slot-aligned with `backends`.
    #[must_use]
    pub fn receipts(&self) -> &[BackendResolutionReceipt] {
        &self.receipts
    }

    /// The backend at `slot`, with its receipt.
    #[must_use]
    pub fn backend(&self, slot: usize) -> Option<ResolvedBackend<'_>> {
        match (self.config.backends.get(slot), self.receipts.get(slot)) {
            (Some(backend), Some(receipt)) => Some(ResolvedBackend {
                slot,
                backend,
                receipt,
            }),
            _ => None,
        }
    }

    /// Every backend, zipped with its receipt, in slot order.
    pub fn backends(&self) -> impl Iterator<Item = ResolvedBackend<'_>> {
        self.config
            .backends
            .iter()
            .zip(self.receipts.iter())
            .enumerate()
            .map(|(slot, (backend, receipt))| ResolvedBackend {
                slot,
                backend,
                receipt,
            })
    }

    /// The backend [`Config::select_configured_backend`] would pick, WITH
    /// its receipt — the same shared index selector, so the two can never
    /// disagree about which backend was selected.
    #[must_use]
    pub fn selected_backend(&self) -> Option<ResolvedBackend<'_>> {
        self.config
            .selected_configured_slot()
            .and_then(|slot| self.backend(slot))
    }
}

/// Dedicated configuration for the compression summarizer, loaded from
/// `~/.newt/summarizer.toml` (Step 24.10, #559). An absent file means
/// `SummarizerConfig::default()` — every field falls back to the session
/// backend, so behavior is unchanged from "summarizer reuses the session
/// model".
///
/// The point of the separate file is the **own-backend** fields
/// (`endpoint`/`model`/`kind`/`api_key_file`): a summarizer can run on a
/// different, fast box than the session model instead of contending with it
/// (the #548 field incident — a slow primary summarizer stalled ~189s before
/// the static marker). `timeout_secs` / `retries` / `fallback_model` are the
/// knobs that used to live under `[tui]` (moved here in 24.10).
///
/// Example `~/.newt/summarizer.toml`:
/// ```toml
/// endpoint = "http://REDACTED-HOST:11434"  # default: session backend URL
/// model    = "qwen2.5-coder:3b"            # default: session model
/// kind     = "ollama"                      # "ollama" | "openai"
/// timeout_secs   = 45
/// retries        = 1
/// fallback_model = "nemotron-mini:4b"      # else preference-list auto-pick (24.9)
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SummarizerConfig {
    /// Summarizer endpoint URL. `None` ⇒ reuse the session backend's URL.
    pub endpoint: Option<String>,
    /// Summarizer model. `None` ⇒ reuse the session backend's model.
    pub model: Option<String>,
    /// Backend protocol. `None` ⇒ reuse the session backend's kind.
    pub kind: Option<BackendKind>,
    /// For `kind = "embedded"` (#661 group C): the local GGUF model file for the
    /// in-process candle summarizer. Ignored for HTTP backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_path: Option<String>,
    /// Bearer-token file (first non-empty line). `None` ⇒ reuse the session key.
    pub api_key_file: Option<String>,
    /// Bearer-token environment variable (checked before `api_key_file`).
    pub api_key_env: Option<String>,
    /// Per-request timeout (seconds). Default 60 — cold-loading a big model can
    /// legitimately exceed it; raise on a slow box that falls back to the marker.
    #[serde(default = "default_summarizer_timeout_secs")]
    pub timeout_secs: u64,
    /// Retry attempts before the static marker. Default 1 — each attempt can
    /// cost the full `timeout_secs` (the #548 189s incident was 3 × 60s).
    #[serde(default = "default_summarizer_retries")]
    pub retries: u32,
    /// Explicit fallback model. `None` ⇒ for an Ollama summarizer backend, the
    /// first installed small-model-preference-list entry is auto-picked (24.9).
    pub fallback_model: Option<String>,
    /// `keep_alive` for the warm + summary requests. `None` ⇒ inherit
    /// `[tui].keep_alive`.
    pub keep_alive: Option<String>,
}

impl Default for SummarizerConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            model: None,
            kind: None,
            model_path: None,
            api_key_file: None,
            api_key_env: None,
            timeout_secs: default_summarizer_timeout_secs(),
            retries: default_summarizer_retries(),
            fallback_model: None,
            keep_alive: None,
        }
    }
}

impl SummarizerConfig {
    /// Parse a `summarizer.toml` body. Pure — fully unit-testable without disk.
    pub fn from_toml_str(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| NewtError::Config(e.to_string()))
    }

    /// Load `~/.newt/summarizer.toml` (or `$NEWT_SUMMARIZER_CONFIG`). A missing
    /// file is not an error — it yields [`SummarizerConfig::default`] (reuse the
    /// session backend). Only a present-but-malformed file errors.
    pub fn resolve() -> Result<Self> {
        for path in Self::candidate_paths() {
            if path.is_file() {
                let text = std::fs::read_to_string(&path)?;
                return Self::from_toml_str(&text);
            }
        }
        Ok(Self::default())
    }

    /// Ordered candidate paths for `summarizer.toml`.
    fn candidate_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Ok(p) = std::env::var("NEWT_SUMMARIZER_CONFIG") {
            paths.push(PathBuf::from(p));
        }
        if let Some(dir) = Config::user_config_dir() {
            paths.push(dir.join("summarizer.toml"));
        }
        paths
    }

    /// Resolve this summarizer's bearer token (env var first, then file —
    /// plaintext or encrypted), or `None` — the same
    /// [`resolve_api_key_common`] rule as [`BackendConfig::resolve_api_key`]
    /// (the mirrored body it used to carry is gone).
    pub fn resolve_api_key(&self) -> Option<String> {
        match resolve_api_key_common(self.api_key_env.as_deref(), self.api_key_file.as_deref()) {
            Ok(v) => return v,
            Err(e) => {
                crate::secrets::warn_once(self.api_key_file.as_deref().unwrap_or("summarizer"), &e);
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

/// The backend the shared precedence selected — a configured `[[backends]]`
/// entry or a `[[providers]]` plugin. Returned inside [`SelectionOutcome`] by
/// [`Config::select_backend`] so every surface resolves ONE backend, then
/// instantiates exactly that one.
///
/// Not `PartialEq`/`Eq`: it borrows [`BackendConfig`] (which carries an `f64`
/// field, so it cannot be `Eq`) and [`ProviderConfig`]. Compare on an owned
/// projection (a name/endpoint), not on the borrow.
#[derive(Debug, Clone)]
pub enum SelectedBackend<'a> {
    Configured(&'a BackendConfig),
    Provider(&'a ProviderConfig),
}

/// The outcome of the shared backend-selection contract
/// ([`Config::select_backend`]). Three cases — kept distinct so the caller
/// cannot collapse an operator error into a silent fallback:
///
/// - [`Selected`](Self::Selected): the precedence picked a concrete backend
///   or provider — instantiate exactly that one.
/// - [`UnknownNamed`](Self::UnknownNamed): an *explicit* selector
///   (`$NEWT_PROVIDER` or `default_backend`) named an entry that matches **no**
///   configured backend or provider. This is an operator error (a typo in the
///   selector), NOT a cue to run some other backend. The caller MUST surface it
///   rather than fall back — otherwise a mistyped `$NEWT_PROVIDER` silently
///   runs the wrong model (invariant: an explicitly selected backend is
///   authoritative; no silent fallback).
/// - [`Unset`](Self::Unset): nothing was explicitly selected and nothing
///   configured qualified. Only here may the caller fall back to local
///   discovery.
///
/// Not `PartialEq`/`Eq` for the same reason as [`SelectedBackend`]; match on it
/// or compare an owned projection.
#[derive(Debug, Clone)]
pub enum SelectionOutcome<'a> {
    /// The precedence selected this backend/provider.
    Selected(SelectedBackend<'a>),
    /// An explicit selector named something that matches no configured entry.
    UnknownNamed(String),
    /// An explicit selector (`$NEWT_PROVIDER` / `default_backend`) named a
    /// configured backend that has NEITHER an endpoint NOR an embedded
    /// `model_path` — there is nothing to route to, and an explicit
    /// selection is authoritative, so silently running some other backend
    /// is as wrong as it is for `UnknownNamed`. The caller MUST surface it:
    /// fix the backend (give it a destination) or the selector.
    UnroutableNamed(String),
    /// Nothing explicitly selected and nothing configured qualified.
    Unset,
}

// ---------------------------------------------------------------------------
// Default
// ---------------------------------------------------------------------------

/// The last-resort localhost Ollama backend: used both as `Config::default()`'s
/// sole backend (no config file at all) and as the [`Config::resolve`] fallback
/// when neither inline `[[backends]]` nor per-file drop-ins supply any, so a
/// bare install still talks to a local Ollama.
fn fallback_localhost_backend() -> BackendConfig {
    BackendConfig {
        name: "ollama".into(),
        endpoint: "http://127.0.0.1:11434".into(),
        model: Some("llama3.1:8b".into()),
        tiers: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
        kind: Some(BackendKind::Ollama),
        ..Default::default()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backends: vec![fallback_localhost_backend()],
            backend_fallback: true,
            default_backend: None,
            discovery: Discovery::default(),
            network: NetworkConfig::default(),
            providers: Vec::new(),
            scratch: None,
            default_tier_order: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
            lifecycle: None,
            dgx: None,
            tui: None,
            shell: None,
            intake: None,
            context: None,
            tools: None,
            tenacity: None,
            tool_exposure: None,
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
            crews: std::collections::BTreeMap::new(),
            crew: None,
            plan: None,
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
    /// 3. `$NEWT_CONFIG_DIR/config.toml` or `~/.newt/config.toml`
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
    /// True when no operator-supplied inference backend exists anywhere: no
    /// inline `[[backends]]` in any config file, no `backends/*.toml`
    /// drop-in, no `[[providers]]`, no `--backend-*` CLI override — the
    /// backend list is exactly the compiled-in localhost fallback (which is
    /// synthesized ONLY in that fully-bare case; a provider-only config is
    /// configured and gets no synthetic backend). This is the first-run wizard's
    /// "nothing configured" predicate: [`Config::resolve`] otherwise silently
    /// invents a localhost Ollama, so a missing config was never observable
    /// as a state. Meaningful only on a config produced by `resolve()`.
    #[must_use]
    pub fn is_unconfigured(&self) -> bool {
        self.backend_fallback
    }

    pub fn resolve() -> Result<Self> {
        Self::resolve_runtime().map(ResolvedConfig::into_config)
    }

    /// [`Config::resolve_runtime_unpublished`] plus the process-global
    /// publication — the full runtime resolution, receipts kept.
    ///
    /// # Errors
    /// Any config-load error `resolve` itself would surface.
    pub fn resolve_runtime() -> Result<ResolvedConfig> {
        let resolved = Self::resolve_runtime_unpublished()?;
        resolved.publish_runtime_settings();
        Ok(resolved)
    }

    /// The full disk resolution WITH per-backend provenance receipts and
    /// WITHOUT the process-global publication: file layering, then the
    /// backend assembly (identity validation → operator drop-in replacement
    /// → exact probe observation → CLI request), receipts aligned 1:1 with
    /// `backends`. For consumers that must resolve and VALIDATE (backend
    /// pick, card binding, principal decision) before anything touches
    /// process-global state — publish explicitly afterwards.
    ///
    /// # Errors
    /// Any config-load error `resolve` itself would surface.
    pub fn resolve_runtime_unpublished() -> Result<ResolvedConfig> {
        let base_path = Self::candidate_paths().into_iter().find(|p| p.is_file());
        // #1301 trust boundary: is the chosen base the AMBIENT cwd-relative
        // `./newt.toml` fallthrough (a freshly cloned repo can ship one at its
        // root — `cd repo && newt` → same host-RCE class as the walk-up) rather
        // than an operator-explicit base? `$NEWT_CONFIG` pins a base at this
        // resolution layer (interactive `--config` publishes that env;
        // explicit-profile consumers may instead call `Config::load`). If it
        // points AT `./newt.toml`, that is the operator's explicit choice
        // (Trusted). Every other `./newt.toml` base is ambient → Untrusted.
        let base_ambient = base_is_ambient_newt_toml(base_path.as_deref());
        // A project-local config that *is* the base (e.g. cwd is the project and
        // its `.newt/config.toml` already matched) must not be merged onto itself.
        let project_path =
            Self::project_config_path().filter(|p| Some(p.as_path()) != base_path.as_deref());

        let mut cfg = match (&base_path, &project_path) {
            // Fast path: no project override.
            (Some(p), None) => {
                if base_ambient {
                    // config-plane-provenance: an AMBIENT `./newt.toml` base is
                    // attacker-reachable (a cloned repo ships one at its root), so
                    // strip its control-plane keys before deserialize — the same
                    // fail-closed treatment the walk-up overlay already gets, and
                    // the base vector the convergence audit surfaced. A
                    // `$NEWT_CONFIG`-pinned / user-home / `/etc` base is
                    // operator-explicit (Trusted) and loaded verbatim.
                    let mut base_val = Self::load_value(p)?;
                    strip_control_plane(&mut base_val);
                    base_val
                        .try_into()
                        .map_err(|e| NewtError::Config(e.to_string()))?
                } else {
                    Self::load(p)?
                }
            }
            (None, None) => Self::default(),
            // Project override present → layer it over the base (or the default
            // config when there is no base file).
            (base, Some(proj)) => {
                let mut merged = match base {
                    Some(p) => Self::load_value(p)?,
                    None => toml::Value::try_from(Self::default())
                        .map_err(|e| NewtError::Config(e.to_string()))?,
                };
                // config-plane-provenance: an ambient `./newt.toml` base is
                // untrusted, so strip its control-plane keys too (the overlay is
                // stripped separately by `merge_project_overlay` below).
                if base_ambient {
                    strip_control_plane(&mut merged);
                }
                let project_val = Self::load_value(proj)?;
                // The merge strategy is itself config: the project declares how
                // it wants to be merged (`[merge] arrays = ...`), else the global
                // config's setting, else the built-in default (Replace).
                let strategy = array_merge_strategy(&project_val, &merged);
                // #1301 trust boundary: a project-local `.newt/config.toml` is
                // found by walking UP from cwd, so a freshly cloned repo can ship
                // one — its `[[mcp_servers]]` are attacker-reachable and must be
                // UNTRUSTED (literals verbatim, `${cmd:…}` never runs, refs
                // rejected), exactly like a `.mcp.json` overlay. The merge below
                // folds those entries into `mcp_servers`, indistinguishable from
                // the trusted base's; capture how many project entries there are
                // BEFORE the merge so provenance can be reconstructed after.
                let project_mcp_count = project_val
                    .get("mcp_servers")
                    .and_then(toml::Value::as_array)
                    .map(Vec::len);
                // config-plane-provenance: the walked-up project overlay is
                // attacker-reachable, so strip its control-plane keys (exec /
                // endpoint authority) BEFORE folding it in — a hostile repo
                // cannot run a command or redirect endpoints via config. Its
                // `[[mcp_servers]]` are left to fold (the count above), then
                // stamped Untrusted by `mark_project_mcp_untrusted` below.
                merge_project_overlay(&mut merged, project_val, strategy);
                let mut cfg: Self = merged
                    .try_into()
                    .map_err(|e| NewtError::Config(e.to_string()))?;
                // `trust` is `#[serde(skip)]`, so every merged entry deserialized
                // to the `Trusted` default; stamp the project-origin ones back to
                // UNTRUSTED before anything can resolve their secrets.
                mark_project_mcp_untrusted(&mut cfg.mcp_servers, strategy, project_mcp_count);
                cfg
            }
        };
        // #1301 trust boundary (ambient `./newt.toml` base): when the base is the
        // cwd-relative `./newt.toml` fallthrough it is itself attacker-reachable,
        // so EVERY entry it contributed — plus any ambient project overlay already
        // merged on top — is UNTRUSTED. This only ever downgrades; a trusted base
        // (user home config, `/etc`, or an explicit `$NEWT_CONFIG`) is untouched
        // and its project overlay was handled by `mark_project_mcp_untrusted`.
        if base_ambient {
            for entry in &mut cfg.mcp_servers {
                entry.trust = crate::mcp::McpTrust::Untrusted;
            }
        }
        // `backend_fallback` provenance: serde never round-trips the skipped
        // flag reliably, so recompute it at the file boundary. A config file
        // that supplied inline `[[backends]]` is operator-configured; a file
        // with none is (so far) as bare as no file at all. The `(None, None)
        // => Self::default()` arm keeps the flag `Default` set (true).
        if base_path.is_some() || project_path.is_some() {
            cfg.backend_fallback = cfg.backends.is_empty();
        }
        // Per-file backends (the endpoint control surface): drop a
        // `~/.newt/backends/<name>.toml` to add/override a backend — no
        // `config.toml` edit, and no overlapping inline `[[backends]]` to
        // hand-deconflict. Runs first so disk loadouts/crews can name a disk
        // backend's provider.
        // The backend assembly: identity validation FIRST (name-keyed
        // machinery is unsound on duplicate/empty names), then per-file
        // drop-ins from the `backends/` dirs next to the config —
        // `~/.newt/backends/*.toml` first, then the project
        // `.newt/backends/` (so project overrides home overrides inline
        // `[[backends]]`).
        let mut assembly =
            BackendAssembly::new(std::mem::take(&mut cfg.backends)).map_err(NewtError::Config)?;
        validate_provider_names(&cfg.providers).map_err(NewtError::Config)?;
        if let Some(dir) = Self::user_config_dir() {
            assembly
                .merge_dir(&dir.join("backends"))
                .map_err(NewtError::Config)?;
        }
        if let Some(proj) = Self::project_config_path() {
            if let Some(parent) = proj.parent() {
                assembly
                    .merge_dir(&parent.join("backends"))
                    .map_err(NewtError::Config)?;
            }
        }
        // A successfully merged operator drop-in is operator-supplied
        // configuration — the resolved backend list is no longer the bare
        // compiled-in fallback (see `is_unconfigured`).
        if assembly.operator_configured() {
            cfg.backend_fallback = false;
        }
        // Localhost fallback: a config that declared no inline `[[backends]]`
        // deserializes to empty (see the field doc); if no drop-in supplied one
        // either — AND no `[[providers]]` exist — restore the bare-install
        // localhost Ollama so newt still has a backend to talk to. A
        // provider-only config is CONFIGURED: synthesizing a backend here
        // would outrank the provider (a Configured pick precedes
        // `providers.first`), silently deserting the operator's provider on
        // the normal path while the profile path selected it.
        if assembly.is_empty() {
            if cfg.providers.is_empty() {
                cfg.backend_fallback = true;
                assembly.push_fallback(fallback_localhost_backend());
            } else {
                cfg.backend_fallback = false;
            }
        }
        // Per-file bundles (the model-support-kit control surface): drop a
        // `~/.newt/bundles/<name>.toml` to add a bundle — no `config.toml` edit.
        cfg.merge_disk_bundles();
        // Per-file loadouts (the shareable composition control surface): drop a
        // `~/.newt/loadouts/<name>.toml` to add a loadout — no `config.toml` edit.
        // Runs after bundles so a disk loadout may name a disk bundle.
        cfg.merge_disk_loadouts();
        // Per-file crews (the role-ensemble control surface): drop a
        // `~/.newt/crews/<name>.toml` to add a crew — no `config.toml` edit.
        // Runs after loadouts so a disk crew may name a disk loadout.
        cfg.merge_disk_crews();
        // Per-file DGX nodes (the per-host control surface): drop a
        // `~/.newt/dgx/<name>.toml` to add/override a DGX node — each host its
        // own file, no inline `[[dgx.nodes]]`. The active selection
        // (active_node/active_endpoint/active_model) stays in `[dgx]`.
        cfg.merge_disk_dgx_nodes();
        // The explicit per-invocation CLI `--backend-*` request — the single
        // owner of that precedence, so an explicit config file cannot defeat
        // a per-invocation backend pin. Recorded AS a request on the target
        // slot's receipt, never blended into the declaration.
        let over = cli_backend_override();
        let requested_slot = assembly
            .apply_request(over.clone(), cfg.default_backend.as_deref())
            .map_err(NewtError::Config)?;
        if assembly.requested() {
            cfg.backend_fallback = false;
        }
        let (backends, receipts) = assembly.finish();
        cfg.backends = backends;
        pin_requested_selection(&mut cfg, over.as_ref(), requested_slot);
        Ok(ResolvedConfig {
            config: cfg,
            receipts,
        })
    }

    /// Apply one final resolved configuration to the runtime: the CLI
    /// `--backend-*` override (through [`BackendOverride::apply`] — the
    /// invariant-owning composer, which warns and leaves the config
    /// untouched on a refused request), then the process-global
    /// publications. Compatibility surface for consumers that hold a bare
    /// [`Config`] (e.g. [`Config::load`] of an explicit profile) and do not
    /// need provenance receipts. Consumers that must validate before any
    /// process-global mutation use [`Config::prepare_runtime`] /
    /// [`Config::resolve_runtime_unpublished`] and publish explicitly.
    pub fn apply_runtime_settings(&mut self) {
        if let Some(over) = cli_backend_override().filter(|o| !o.is_empty()) {
            over.apply(self);
        }
        self.publish_runtime_settings();
    }

    /// Prepare an already-loaded config (an explicit `--profile` file, a
    /// constructed config) for the runtime WITHOUT touching process-global
    /// state: run the backend assembly over its backends — identity
    /// validation, then the CLI `--backend-*` request — and return the
    /// receipt-bearing [`ResolvedConfig`]. No disk drop-ins are merged and
    /// no localhost fallback is invented: the profile IS the whole config.
    /// Publish explicitly afterwards.
    ///
    /// # Errors
    /// Duplicate or empty backend names; a CLI request naming an ambiguous
    /// backend.
    pub fn prepare_runtime(mut self) -> Result<ResolvedConfig> {
        let mut assembly =
            BackendAssembly::new(std::mem::take(&mut self.backends)).map_err(NewtError::Config)?;
        validate_provider_names(&self.providers).map_err(NewtError::Config)?;
        let default_backend = self.default_backend.clone();
        let over = cli_backend_override();
        let requested_slot = assembly
            .apply_request(over.clone(), default_backend.as_deref())
            .map_err(NewtError::Config)?;
        if assembly.requested() {
            self.backend_fallback = false;
        }
        let (backends, receipts) = assembly.finish();
        self.backends = backends;
        pin_requested_selection(&mut self, over.as_ref(), requested_slot);
        Ok(ResolvedConfig {
            config: self,
            receipts,
        })
    }

    /// Publish the resolved configuration's process-global settings. Keep
    /// this AFTER any validation that should be able to fail without leaving
    /// half-published globals behind. Reads only (`&self`): publication
    /// copies resolved values into process-global slots, it never edits the
    /// config.
    pub fn publish_runtime_settings(&self) {
        // #726: push the resolved `[tools] max_output_tokens` into the
        // process-wide model-facing output budget without threading a new
        // `usize` through `ChatCtx` + `execute_tool` + every call site.
        crate::agentic::set_max_output_tokens(self.max_output_tokens());
        crate::agentic::set_output_head_tokens(self.output_head_tokens());
        crate::agentic::set_output_cap_chars_per_token(self.output_cap_chars_per_token());
        // #tenacity: publish the resolved `[tenacity]` config so
        // `effective_tenacity` can pick the per-family default for the active
        // model. An explicit `--tenacity` still supersedes it.
        crate::tenacity::set_tenacity_config(self.tenacity.clone().unwrap_or_default());
        // #880: publish the repo `[lifecycle]` overrides so the crew's normalize
        // (and future phase consumers) honor them.
        if let Some(lc) = &self.lifecycle {
            crate::tooling::set_lifecycle_override(lc.clone());
        }
        // #844: publish `[scratch] dir` so crew worktrees / the crew target /
        // session plans honor it. `NEWT_SCRATCH_DIR` still overrides.
        if let Some(dir) = self.scratch.as_ref().and_then(|s| s.dir.as_deref()) {
            crate::scratch::set_scratch_dir(dir);
        }
        // #1789: publish `[network] owned_suffixes` so retry policy can treat
        // operator-owned inference hosts as patiently as loopback ones.
        if !self.network.owned_suffixes.is_empty() {
            crate::owned_hosts::set_owned_suffixes(self.network.owned_suffixes.clone());
        }
    }

    /// The configured model-facing output token budget (`[tools]
    /// max_output_tokens`), or the built-in default when `[tools]` is absent
    /// (#726). `0` means "no cap". See [`ToolsConfig`].
    pub fn max_output_tokens(&self) -> usize {
        self.tools
            .as_ref()
            .map(|t| t.max_output_tokens)
            .unwrap_or_else(default_max_output_tokens)
    }

    /// The resolved `[tool_exposure]` policy, or the identity (`Full`) default
    /// when the section is absent. See [`ToolExposureConfig`].
    pub fn tool_exposure(&self) -> ToolExposureConfig {
        self.tool_exposure.unwrap_or_default()
    }

    /// The configured head allocation for oversized `run_command` output
    /// (`[tools] output_head_tokens`), or the built-in tail-biased default.
    pub fn output_head_tokens(&self) -> usize {
        self.tools
            .as_ref()
            .map(|t| t.output_head_tokens)
            .unwrap_or_else(default_output_head_tokens)
    }

    /// The resolved `[tools] output_cap_chars_per_token` — the conservative
    /// chars/token used to size the model-facing output cap — or the built-in
    /// default (3) when `[tools]` is absent. See [`ToolsConfig`].
    pub fn output_cap_chars_per_token(&self) -> usize {
        self.tools
            .as_ref()
            .map(|t| t.output_cap_chars_per_token)
            .unwrap_or_else(default_output_cap_chars_per_token)
    }

    /// Validate backend IDENTITY before any name-keyed machinery runs:
    /// every backend needs a nonempty name, and no two may share one.
    /// Selection (`default_backend`, `$NEWT_PROVIDER`), CLI overrides,
    /// drop-in merging, and the provenance receipts are all name-keyed —
    /// with a duplicate, receipts collapse last-wins while selection takes
    /// the first match, which can hand backend A the card binding declared
    /// for backend B and activate the wrong card. Hard, actionable error
    /// instead.
    pub fn validate_backend_identities(&self) -> std::result::Result<(), String> {
        validate_backend_names(self.backends.iter())
    }

    /// Merge per-file DGX nodes from the `dgx/` dirs next to the config:
    /// `~/.newt/dgx/*.toml` first, then the project `.newt/dgx/` (so project
    /// overrides home overrides inline `[[dgx.nodes]]`). Filename stem = node
    /// name. A malformed drop-in is skipped with a warning; it must not break
    /// startup.
    fn merge_disk_dgx_nodes(&mut self) {
        if let Some(dir) = Self::user_config_dir() {
            self.merge_dgx_nodes_from_dir(&dir.join("dgx"));
        }
        if let Some(proj) = Self::project_config_path() {
            if let Some(parent) = proj.parent() {
                self.merge_dgx_nodes_from_dir(&parent.join("dgx"));
            }
        }
    }

    /// Load `<dir>/*.toml` as DGX nodes (filename stem = name) into
    /// `self.dgx.nodes`. A drop-in **replaces** an existing node of the same
    /// name (last-wins), else it is appended — so a `dgx1.toml` file supersedes
    /// an inline `[[dgx.nodes]]` named `dgx1` without a duplicate. The `[dgx]`
    /// table is created (default selection) if it was absent. A malformed file
    /// is skipped with a warning.
    fn merge_dgx_nodes_from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // no dgx dir — fine
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
            match std::fs::read_to_string(&path).map(|t| toml::from_str::<crate::dgx::DgxNode>(&t))
            {
                Ok(Ok(mut node)) => {
                    // The filename is authoritative for the name (collision-free).
                    node.name = stem.to_string();
                    let dgx = self.dgx.get_or_insert_with(Default::default);
                    match dgx.nodes.iter_mut().find(|n| n.name == node.name) {
                        Some(existing) => *existing = node,
                        None => dgx.nodes.push(node),
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping malformed dgx node file");
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

    /// The user-writable config root: `$NEWT_CONFIG_DIR` or `~/.newt`.
    /// The operator-PINNED config path (`$NEWT_CONFIG`), when set — the one
    /// base whose sibling `models/` is a legitimate card source. Pure env
    /// mirror of `candidate_paths`' first entry; deliberately NOT "whatever
    /// base resolve happened to pick", because an ambient `./newt.toml` base
    /// is attacker-reachable (#1301) and its sibling `./models/` must never
    /// satisfy a trusted backend's card pointer.
    #[must_use]
    pub fn pinned_config_path() -> Option<PathBuf> {
        std::env::var("NEWT_CONFIG")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            // A set-but-missing NEWT_CONFIG falls through to the next
            // candidate at load — so it must not redirect the card catalog
            // either, or cards would resolve relative to a config that was
            // never actually selected.
            .filter(|p| p.is_file())
    }

    pub fn user_config_dir() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os(NEWT_CONFIG_DIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
        {
            return Some(path);
        }
        home_dir().map(|h| h.join(".newt"))
    }

    /// The user-writable config path: `$NEWT_CONFIG_DIR/config.toml` or
    /// `~/.newt/config.toml`.
    /// This is the first path `resolve()` reads and the target for `save()`.
    pub fn user_config_path() -> Option<PathBuf> {
        Self::user_config_dir().map(|dir| dir.join("config.toml"))
    }

    /// The shared config-based backend-selection precedence (#1320, PR-3) — the
    /// single definition used by chat's `resolve_backend_choice` config rungs, by
    /// `solve`, and by the ACP worker, so every entry point agrees on which
    /// configured backend the operator named. Most-specific first: `$NEWT_PROVIDER`
    /// names a backend > `default_backend` > a sole backend > prefer an
    /// OpenAI-kind entry, else the first routable one — where routable means a
    /// nonempty endpoint OR an embedded `model_path` ([`backend_is_routable`]).
    ///
    /// `None` in exactly three cases: no configured backend has a
    /// destination; an EXPLICIT selector (`$NEWT_PROVIDER` /
    /// `default_backend`) names a configured but destination-less backend;
    /// or an explicit selector names NOTHING configured. This `Option`
    /// surface cannot carry those errors, so it selects NOTHING rather than
    /// silently selecting the unroutable backend or silently running some
    /// other one (both pre-#1819 behaviors); the typed errors are
    /// [`SelectionOutcome::UnroutableNamed`] /
    /// [`SelectionOutcome::UnknownNamed`] on [`Config::select_backend`].
    /// Env-synthesized fallbacks (codex, legacy dgx, localhost) stay in chat's
    /// `resolve_backend_choice`, layered around this.
    #[must_use]
    pub fn select_configured_backend(&self) -> Option<&BackendConfig> {
        self.selected_configured_slot().map(|i| &self.backends[i])
    }

    /// The index behind [`Config::select_configured_backend`] and
    /// [`ResolvedConfig::selected_backend`] — the shared
    /// [`select_backend_slot`], so the borrowed pick, the receipt-bearing
    /// pick, and the CLI's unnamed field-only targeting can never disagree.
    fn selected_configured_slot(&self) -> Option<usize> {
        let backends: Vec<&BackendConfig> = self.backends.iter().collect();
        match select_backend_slot(&backends, self.default_backend.as_deref()) {
            SlotSelection::Slot(i) => Some(i),
            // The Option surfaces select NOTHING here — see the
            // `select_configured_backend` doc; `select_backend` carries the
            // typed `UnknownNamed` / `UnroutableNamed` errors.
            SlotSelection::ExplicitlyUnroutable { .. }
            | SlotSelection::ExplicitlyUnmatched { .. }
            | SlotSelection::None => None,
        }
    }

    /// The ONE backend-selection contract, unified across `[[backends]]` and
    /// `[[providers]]`, so an explicitly named backend is authoritative on every
    /// surface (chat / solve / worker). Precedence, most-specific first:
    /// `$NEWT_PROVIDER` (names a backend OR a provider) > `default_backend`
    /// (either) > a sole configured backend > the preference rules of
    /// [`Self::select_configured_backend`] > a sole/first provider.
    ///
    /// Returns a [`SelectionOutcome`]: `Selected` when the precedence picks a
    /// concrete backend/provider; `UnknownNamed` when an *explicit* selector
    /// names something that matches nothing configured (an operator error — the
    /// caller must NOT fall back to a different backend); `UnroutableNamed`
    /// when an explicit selector names a configured backend with neither
    /// endpoint nor model_path (equally an operator error — nothing to route
    /// to, and no silent fallback); `Unset` only when nothing is explicitly
    /// selected and nothing configured qualifies, at which point the caller
    /// may fall back to local discovery.
    ///
    /// A provider is chosen only when the precedence selects it: a bare
    /// `providers.first()` never bypasses `$NEWT_PROVIDER` / `default_backend`
    /// (the ACP-worker bug this closes).
    pub fn select_backend(&self) -> SelectionOutcome<'_> {
        // The most-specific PRESENT selector decides — `$NEWT_PROVIDER` if set,
        // else `default_backend`. Only that one selector is consulted: if it is
        // set but names nothing, we must NOT fall through to the next selector or
        // to preference (either would be a silent fallback). A mistyped
        // `$NEWT_PROVIDER` is an error, not permission to run `default_backend`.
        let explicit_selector = std::env::var("NEWT_PROVIDER")
            .ok()
            .filter(|n| !n.is_empty())
            .or_else(|| self.default_backend.clone().filter(|n| !n.is_empty()));
        if let Some(name) = explicit_selector {
            // A routable backend claims this name → fall through to the shared
            // precedence below (which re-checks `$NEWT_PROVIDER` / `default_backend`
            // and selects exactly that backend). Backends win a name tie.
            let routable_backend = self
                .backends
                .iter()
                .any(|b| b.name == name && backend_is_routable(b));
            if !routable_backend {
                // A provider claims this name → select it (an unroutable
                // backend does not win a name tie against a provider).
                if let Some(provider) = self.providers.iter().find(|p| p.name == name) {
                    return SelectionOutcome::Selected(SelectedBackend::Provider(provider));
                }
                // A destination-less backend claims it → an operator error:
                // the explicit selection is authoritative, there is nothing
                // to route to, and silently running another backend is the
                // exact failure mode `UnknownNamed` exists for.
                if self.backends.iter().any(|b| b.name == name) {
                    return SelectionOutcome::UnroutableNamed(name);
                }
                // Nothing configured claims it at all.
                return SelectionOutcome::UnknownNamed(name);
            }
        }
        // The shared backend precedence (sole > prefer-openai > first usable).
        if let Some(backend) = self.select_configured_backend() {
            return SelectionOutcome::Selected(SelectedBackend::Configured(backend));
        }
        // Nothing in [[backends]] qualified: a sole/first provider, else Unset.
        match self.providers.first() {
            Some(provider) => SelectionOutcome::Selected(SelectedBackend::Provider(provider)),
            None => SelectionOutcome::Unset,
        }
    }

    /// Serialize the config to pretty TOML for **audit**, with inline secret
    /// material redacted. Every `[[mcp_servers]]` `env` / `headers` **literal**
    /// value is replaced with [`Self::REDACTED`] — a literal is the only place
    /// `Config` can carry a raw secret inline (e.g. an `Authorization: Bearer …`
    /// header, an `API_KEY=…` child env var, or a `Bearer ${cmd:…}`
    /// interpolation string that could embed literal secret text). Keys are kept
    /// so an auditor sees *which* variables/headers are set without the values.
    ///
    /// Secret *references* — a `{ env | file | cmd }` [`SecretValue::Ref`], or an
    /// `api_key_file` / `api_key_env` — are left as-is: they name *where* a
    /// secret lives, never the secret itself, so `newt config` can show the
    /// operator their wiring without ever printing a resolved value.
    ///
    /// # Errors
    /// A TOML serialization failure (should not happen for a valid `Config`).
    pub fn to_redacted_toml(&self) -> Result<String> {
        use crate::mcp::SecretValue;
        let mut redacted = self.clone();
        let redact = |v: &mut SecretValue| {
            if matches!(v, SecretValue::Literal(_)) {
                *v = SecretValue::Literal(Self::REDACTED.to_string());
            }
        };
        for server in &mut redacted.mcp_servers {
            server.env.values_mut().for_each(redact);
            server.headers.values_mut().for_each(redact);
            // A `url` can embed userinfo (`user:pass@`) or a `?api_key=…` param,
            // and `args` can carry a `--token …` — none of which are `SecretValue`s,
            // so they'd otherwise pass through verbatim into an audit dump (#1301).
            if let Some(url) = &server.url {
                server.url = Some(redact_url_secrets(url));
            }
            server.args = redact_arg_secrets(&server.args);
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
    ///
    /// A configured `[skills].bundled_dir` is appended **last** (lowest
    /// priority), so a user skill of the same name shadows the bundled one.
    #[must_use]
    pub fn skill_search_dirs(&self) -> Vec<PathBuf> {
        let configured = self
            .skills
            .as_ref()
            .map(|s| s.search.as_slice())
            .unwrap_or(&[]);
        let mut dirs: Vec<PathBuf> = if configured.is_empty() {
            let default = Self::user_config_dir()
                .map(|dir| dir.join("skills"))
                .unwrap_or_else(|| PathBuf::from(".newt/skills"));
            vec![default]
        } else {
            configured.iter().map(|s| expand_tilde(s)).collect()
        };

        // Bundled skills scanned last: user-configured dirs win a name
        // collision (first-wins in `discover_paths`), so users can override
        // any bundled skill by shipping their own of the same name.
        if let Some(bundled) = self
            .skills
            .as_ref()
            .map(|s| s.bundled_dir.as_str())
            .filter(|s| !s.is_empty())
        {
            dirs.push(expand_tilde(bundled));
        }

        dirs
    }

    /// Fill in a default `[skills].bundled_dir` when the user left it unset, so
    /// an agent running **inside a newt checkout gets the repo's bundled skills
    /// surfaced out-of-the-box** (progressive-disclosure index → `use_skill`)
    /// without any config. Detection walks up from `cwd` for a
    /// `.newt/bundled-skills` directory; if none is found (or the field is
    /// already set), the config is returned unchanged. Kept off the pure
    /// [`Self::skill_search_dirs`] path — the filesystem probe lives only here.
    ///
    /// This is the smallest first step (dev/agent-in-checkout); packaging a
    /// default bundled dir for an *installed* newt is a follow-up (see the
    /// bundled-skills epic).
    #[must_use]
    pub fn with_bundled_default(mut self) -> Self {
        let already_set = self
            .skills
            .as_ref()
            .is_some_and(|s| !s.bundled_dir.is_empty());
        if already_set {
            return self;
        }
        let Ok(cwd) = std::env::current_dir() else {
            return self;
        };
        if let Some(dir) =
            find_ancestor_dir(&cwd, Path::new(".newt/bundled-skills"), |p| p.is_dir())
        {
            self.skills
                .get_or_insert_with(SkillsConfig::default)
                .bundled_dir = dir.to_string_lossy().into_owned();
        }
        self
    }

    /// The personas directory: sibling of `~/.newt/config.toml`, i.e.
    /// `~/.newt/personas`. Falls back to a relative `./personas` only when
    /// `$HOME` can't be resolved. The same default `newt-tui`'s
    /// `PersonaStore::default_dir()` uses; a headless caller (#1021 PR 5.2)
    /// resolves it the same way without depending on `newt-tui`.
    #[must_use]
    pub fn personas_dir() -> PathBuf {
        Self::user_config_path()
            .map(|p| p.with_file_name("personas"))
            .unwrap_or_else(|| PathBuf::from("personas"))
    }

    /// Serialize this config and write it to `path`, creating parent dirs if needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(NewtError::Io)?;
        }
        let destination = crate::atomic_fs::ResolvedPath::resolve(path).map_err(|error| {
            NewtError::Config(format!(
                "resolve config destination for {}: {error:#}",
                path.display()
            ))
        })?;
        let _lock = crate::atomic_fs::acquire_lock(&destination.lock_path())
            .map_err(|error| NewtError::Config(format!("lock {}: {error:#}", path.display())))?;
        let text = toml::to_string_pretty(self).map_err(|e| NewtError::Config(e.to_string()))?;
        destination
            .atomic_write(text.as_bytes())
            .map_err(|error| NewtError::Config(format!("write {}: {error:#}", path.display())))
    }

    /// Set the top-level `default_backend` key while preserving the rest of the
    /// TOML document, including comments and formatting. Pure: the caller owns
    /// any filesystem write.
    pub fn with_default_backend(text: &str, name: &str) -> Result<String> {
        if name.trim().is_empty() {
            return Err(NewtError::Config(
                "default backend name cannot be empty".to_string(),
            ));
        }

        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| NewtError::Config(format!("config is not valid TOML: {e}")))?;
        let root = doc.as_table_mut();
        if let Some(item) = root.get_mut("default_backend") {
            let existing = item.as_str().ok_or_else(|| {
                NewtError::Config("`default_backend` is not a string".to_string())
            })?;
            if existing == name {
                return Ok(doc.to_string());
            }

            let decor = item
                .as_value()
                .expect("a string item is always a value")
                .decor()
                .clone();
            *item = toml_edit::value(name);
            *item
                .as_value_mut()
                .expect("toml_edit::value always creates a value")
                .decor_mut() = decor;
        } else {
            root.insert("default_backend", toml_edit::value(name));
        }
        Ok(doc.to_string())
    }

    /// Set `enabled` on the named `[[mcp_servers]]` entry, preserving comments
    /// and formatting (`/mcp enable|disable`, #1149). PURE (no I/O) like
    /// [`with_net_host`](Self::with_net_host); the caller does the read/write.
    /// Errors when the server name isn't present.
    pub fn with_mcp_enabled(text: &str, name: &str, enabled: bool) -> Result<String> {
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| NewtError::Config(format!("config is not valid TOML: {e}")))?;
        let servers =
            doc.as_table_mut()
                .entry("mcp_servers")
                .or_insert(toml_edit::Item::ArrayOfTables(
                    toml_edit::ArrayOfTables::new(),
                ));
        let arr = servers.as_array_of_tables_mut().ok_or_else(|| {
            NewtError::Config("[[mcp_servers]] is not an array of tables".to_string())
        })?;
        let entry = arr
            .iter_mut()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
            .ok_or_else(|| NewtError::Config(format!("no [[mcp_servers]] entry named `{name}`")))?;
        if enabled {
            // Default is enabled: remove the key so the file stays minimal.
            entry.remove("enabled");
        } else {
            entry["enabled"] = toml_edit::value(false);
        }
        Ok(doc.to_string())
    }

    /// Append a `[[mcp_servers]]` entry to the TOML `text`, preserving comments
    /// and formatting (`newt mcp add|install`). PURE (no I/O) like
    /// [`with_mcp_enabled`](Self::with_mcp_enabled); the caller does the
    /// read/write. Defaults stay implicit (no `enabled = true`, no
    /// `type = "stdio"`) so the file stays minimal. Errors on an empty name, a
    /// duplicate name, or an entry whose transport is missing its required
    /// field ([`crate::mcp::McpServerEntry::is_valid`]) — an unconnectable
    /// server never lands in the file.
    pub fn with_mcp_server_added(text: &str, entry: &crate::mcp::McpServerEntry) -> Result<String> {
        crate::mcp::validate_entry_for_write(entry)?;
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| NewtError::Config(format!("config is not valid TOML: {e}")))?;
        let servers =
            doc.as_table_mut()
                .entry("mcp_servers")
                .or_insert(toml_edit::Item::ArrayOfTables(
                    toml_edit::ArrayOfTables::new(),
                ));
        let arr = servers.as_array_of_tables_mut().ok_or_else(|| {
            NewtError::Config("[[mcp_servers]] is not an array of tables".to_string())
        })?;
        if arr
            .iter()
            .any(|t| t.get("name").and_then(|v| v.as_str()) == Some(entry.name.as_str()))
        {
            return Err(NewtError::Config(format!(
                "an [[mcp_servers]] entry named `{}` already exists (remove it first, \
                 or toggle it with `/mcp enable|disable`)",
                entry.name
            )));
        }
        arr.push(crate::mcp::entry_to_toml_table(entry, None)?);
        Ok(doc.to_string())
    }

    /// Remove the named `[[mcp_servers]]` entry from the TOML `text`, preserving
    /// comments and formatting (`newt mcp remove`). PURE (no I/O); the caller
    /// does the read/write. Errors when the server name isn't present.
    pub fn with_mcp_server_removed(text: &str, name: &str) -> Result<String> {
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| NewtError::Config(format!("config is not valid TOML: {e}")))?;
        // Distinguish "no section" (→ the entry is absent) from "a section this
        // writer cannot edit" (e.g. the inline-array form, which the serde
        // reader accepts) — misreporting the latter as absent would be a lie.
        let item = doc
            .as_table_mut()
            .get_mut("mcp_servers")
            .ok_or_else(|| NewtError::Config(format!("no [[mcp_servers]] entry named `{name}`")))?;
        let arr = item.as_array_of_tables_mut().ok_or_else(|| {
            NewtError::Config("[[mcp_servers]] is not an array of tables".to_string())
        })?;
        let before = arr.len();
        arr.retain(|t| t.get("name").and_then(|v| v.as_str()) != Some(name));
        if arr.len() == before {
            return Err(NewtError::Config(format!(
                "no [[mcp_servers]] entry named `{name}`"
            )));
        }
        Ok(doc.to_string())
    }

    /// Build the ordered list of candidate config file paths.
    fn candidate_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        if let Ok(p) = std::env::var("NEWT_CONFIG") {
            paths.push(PathBuf::from(p));
        }

        paths.push(PathBuf::from("./newt.toml"));

        if let Some(path) = Self::user_config_path() {
            paths.push(path);
        }

        paths.push(PathBuf::from("/etc/newt/config.toml"));
        paths
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "config_tests/tests.rs"]
mod tests;

/// W1 (unified backend resolution) — the authoritative selection suite for
/// [`Config::select_backend`], covering all eight precedence scenarios the
/// corrective spec names. The companion `newt-acp-worker` suite proves the
/// *destination* (transport + URL) each selected backend instantiates to; this
/// suite proves *which* entry the ONE contract selects.
///
/// Every test is serialized on `newt_provider_env`: `select_backend` reads the
/// process-global `$NEWT_PROVIDER`, so these must never run concurrently with
/// one another (the env-mutating cases would otherwise leak into the env-free
/// ones). Each `$NEWT_PROVIDER` case restores the prior value *before* asserting
/// so a failed assert cannot pollute the next test in the lane.
#[cfg(test)]
#[path = "config_tests/select_backend_tests.rs"]
mod select_backend_tests;

// Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 21:30 EDT | Date: 2026-08-12
