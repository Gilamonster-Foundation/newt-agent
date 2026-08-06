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

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{NewtError, Result};
use crate::router::Tier;
pub use newt_tuner::ModelTuning;
pub use tool_exposure::{ExposureProfile, ToolExposureConfig};
pub use tools::ToolsConfig;

mod shell;
mod tool_exposure;
mod tools;
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

/// One named permission-posture binding (`[modes.<name>]`, retained for
/// compatibility): the atomic binding `/posture <name>` applies.
///
/// ```toml
/// [modes.triage]
/// skill   = "oncall-triage"        # skill body to preload (use_skill path)
/// preset  = "readonly-triage"      # [permission_presets.<name>] to clamp to
/// framing = "On-call triage: investigate, do not change production."
/// ```
///
/// Every field is optional so a posture can do any subset (e.g. preset-only, or
/// framing-only). A `skill`/`preset` that names a missing entry is reported as
/// an error by the command rather than silently ignored — a posture that claims
/// a clamp it never applied would be a false security claim.
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
    /// How strictly the gate ACTS on flagged output — the tier. Default
    /// `RevertRetry` (today's behavior when the `retry` technique is on); lower
    /// tiers (`off`/`advisory`/`revert_once`) trade enforcement for latitude.
    #[serde(default)]
    pub tier: crate::verify_gate::VerifyTier,
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
/// itself (`[<ts> · <model> · <ws> · <mode> ] ❯ `), so the input surface floats
/// it at the bottom while idle (like cargo's progress line) and it doubles as a
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

/// Color / theme mode — the `[tui] color` key and the `--color` CLI flag
/// (issue #527). Selects whether — and eventually how — ANSI color is emitted
/// for the interactive prompt and chat surface. The default is `auto`: color on
/// a TTY, none in pipes / under `NO_COLOR` / `TERM=dumb`.
///
/// `dark`/`light`/`inverted`/`minimal` are accepted and parse today; their
/// palettes are initial mappings (currently the chromatic default) tuned in a
/// later pass. The terminal-aware *resolution* lives in the TUI layer — newt-core
/// has no business probing the terminal — so this enum only exposes the pure
/// pieces ([`from_keyword`](Self::from_keyword) / [`keyword`](Self::keyword) /
/// [`forced`](Self::forced) / [`is_mono`](Self::is_mono)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    /// Color on a TTY; none off one or under `NO_COLOR`/`TERM=dumb` (default).
    #[default]
    Auto,
    /// Always emit color — even off a TTY (screenshots, captured logs). An
    /// explicit `--color=always` also overrides `NO_COLOR` (documented deviation).
    Always,
    /// Never emit color.
    Never,
    /// Reduced color: structure only, no bright accents. (Initial mapping:
    /// chromatic; tuned later.)
    Minimal,
    /// Swapped foreground/background accents for high-contrast terminals.
    /// (Initial mapping: chromatic; tuned later.)
    Inverted,
    /// Palette tuned for a dark background — the current chromatic default.
    Dark,
    /// Palette tuned for a light background. (Initial mapping: chromatic; tuned later.)
    Light,
    /// Force monochrome — no color, ASCII glyph fallbacks. Equivalent to `--mono`.
    Mono,
}

impl ColorMode {
    /// Parse a CLI/config keyword (case-insensitive) into a mode. `on`/`off` are
    /// accepted as aliases of `always`/`never`; `monochrome` aliases `mono`.
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "always" | "on" => Some(Self::Always),
            "never" | "off" => Some(Self::Never),
            "minimal" => Some(Self::Minimal),
            "inverted" => Some(Self::Inverted),
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "mono" | "monochrome" => Some(Self::Mono),
            _ => None,
        }
    }

    /// The canonical lowercase keyword for this mode (round-trips `from_keyword`
    /// and matches the serde representation).
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
            Self::Minimal => "minimal",
            Self::Inverted => "inverted",
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Mono => "mono",
        }
    }

    /// Whether this mode forces a color decision regardless of the terminal:
    /// `Some(true)` = force color on, `Some(false)` = force off, `None` = defer
    /// to terminal detection (`Auto`).
    pub fn forced(self) -> Option<bool> {
        match self {
            Self::Always | Self::Minimal | Self::Inverted | Self::Dark | Self::Light => Some(true),
            Self::Never | Self::Mono => Some(false),
            Self::Auto => None,
        }
    }

    /// Whether color is fully disabled in monochrome form. `Mono` additionally
    /// signals ASCII-glyph fallbacks (`>` for `❯`) to callers; `Never` just
    /// drops color.
    pub fn is_mono(self) -> bool {
        matches!(self, Self::Mono)
    }
}

/// Markdown rendering mode — the `[tui] markdown` key and the `/markdown`
/// command (Step 25.4, #568). This controls RichTUI text output, including
/// assistant replies and built-in Markdown documents such as `/help`. `Auto`
/// renders Markdown whenever color is active; `On`/`Off` force the choice
/// (`On` still needs color to emit ANSI). The effective decision is
/// `mode.forced().unwrap_or(color_on) && color_on`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MarkdownMode {
    /// Render Markdown whenever color is active (default).
    #[default]
    Auto,
    /// Force Markdown rendering on (still gated by color support).
    On,
    /// Disable Markdown rendering — stream raw text.
    Off,
}

impl MarkdownMode {
    /// Parse a CLI/config/command keyword (case-insensitive). `always`/`never`
    /// alias `on`/`off`.
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "on" | "always" => Some(Self::On),
            "off" | "never" => Some(Self::Off),
            _ => None,
        }
    }

    /// The canonical lowercase keyword (round-trips `from_keyword` + serde).
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }

    /// `Some(true)`/`Some(false)` force the decision; `None` (`Auto`) defers to
    /// color detection.
    pub fn forced(self) -> Option<bool> {
        match self {
            Self::On => Some(true),
            Self::Off => Some(false),
            Self::Auto => None,
        }
    }
}

/// Context-management strategy — the `[context] manager` key and the
/// `/context manager <name>` command (Step 24.8, #559). `standard` is the
/// current prune → summary → static-marker pipeline. `progressive` and
/// `distributed` are the retrievable-card managers **owned by #546** and not
/// yet available — selecting them reports that and stays on `standard`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ContextManager {
    /// Prune → summarize → static-marker (today's behavior). The only one
    /// implemented; the selector seam for the others.
    #[default]
    Standard,
    /// Leave a lookup marker; retrieve cards on demand (ephemeral → local DB).
    /// Owned by #546 — not yet available.
    Progressive,
    /// Agent-mesh-shared card store across a swarm. Owned by #546 — not yet
    /// available.
    Distributed,
}

impl ContextManager {
    /// Parse a CLI/config/command keyword (case-insensitive).
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standard" => Some(Self::Standard),
            "progressive" => Some(Self::Progressive),
            "distributed" => Some(Self::Distributed),
            _ => None,
        }
    }

    /// The canonical lowercase keyword (round-trips `from_keyword` + serde).
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Progressive => "progressive",
            Self::Distributed => "distributed",
        }
    }

    /// Whether this manager is implemented. Only `standard` today; the others
    /// are owned by #546 (the selector reports "not yet available").
    pub fn available(self) -> bool {
        matches!(self, Self::Standard)
    }

    /// The default feature bundle this preset turns on (Phase 26, #588). A
    /// preset is a named bundle of composable [`ContextFeature`]s; config and
    /// `/context feature` overrides layer on top (see [`ContextFeatures`]).
    /// Every preset currently resolves to the all-off baseline (today's
    /// `standard` behavior) because no composable feature is implemented yet;
    /// presets gain features as 26.3–26.6 land.
    pub fn base_features(self) -> ContextFeatureSet {
        ContextFeatureSet::default()
    }
}

/// Automatic compaction trigger policy — the `[context]
/// compaction_trigger_policy` key.
///
/// The default is [`HeadroomAware`](Self::HeadroomAware): a message-count
/// threshold is a fallback when Newt does not know the usable input ceiling,
/// rather than a reason to compact a roomy, known context window. Explicit
/// token/send-budget pressure continues to trigger compaction under either
/// policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTriggerPolicy {
    /// Defer a count-only compaction when an authoritative input ceiling is
    /// known. This is the safe default for large-context models.
    #[default]
    HeadroomAware,
    /// Preserve the legacy behavior: compact whenever the message count
    /// exceeds its threshold, even if authoritative input headroom remains.
    MessageCount,
}

impl CompactionTriggerPolicy {
    /// Parse a CLI/config/command keyword (case-insensitive).
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "headroom_aware" => Some(Self::HeadroomAware),
            "message_count" => Some(Self::MessageCount),
            _ => None,
        }
    }

    /// The canonical snake-case keyword (round-trips `from_keyword` + serde).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HeadroomAware => "headroom_aware",
            Self::MessageCount => "message_count",
        }
    }

    /// Alias for [`Self::as_str`], matching the existing context selector API.
    pub const fn keyword(self) -> &'static str {
        self.as_str()
    }
}

/// A composable context-management feature (Phase 26, #588) — an independent
/// on/off technique under `[context.features]` and the `/context feature <name>
/// on|off` command. None are implemented yet (`available()` is false for all);
/// they land in 26.3–26.6 and report "not yet available" until then (same
/// pattern as the `ContextManager` presets under #546).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextFeature {
    /// Cap oversized tool outputs; spill the full payload to a re-readable store (#584).
    ToolOffload,
    /// Structured `<state>` store mutated via tools, kept out of the log (#583).
    Scratchpad,
    /// Structure-aligned retrieval of repo evidence (#582).
    Semantic,
    /// Provenance-preserving compaction with retrievable handles (#584).
    Provenance,
    /// Write-gated cross-task experience memory (#585).
    Experiential,
    /// Per-step compiled context view instead of a rolling buffer (#586).
    Scheduled,
}

impl ContextFeature {
    /// Every feature, in display order.
    pub const ALL: [Self; 6] = [
        Self::ToolOffload,
        Self::Scratchpad,
        Self::Semantic,
        Self::Provenance,
        Self::Experiential,
        Self::Scheduled,
    ];

    /// Parse a keyword (case-insensitive; `-`/`_` interchangeable; short aliases).
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "tool_offload" | "tooloffload" | "offload" => Some(Self::ToolOffload),
            "scratchpad" | "state" => Some(Self::Scratchpad),
            "semantic" | "retrieval" => Some(Self::Semantic),
            "provenance" | "handles" => Some(Self::Provenance),
            "experiential" | "experience" => Some(Self::Experiential),
            "scheduled" | "compiled" => Some(Self::Scheduled),
            _ => None,
        }
    }

    /// The canonical lowercase keyword (matches the `[context.features]` key).
    pub fn keyword(self) -> &'static str {
        match self {
            Self::ToolOffload => "tool_offload",
            Self::Scratchpad => "scratchpad",
            Self::Semantic => "semantic",
            Self::Provenance => "provenance",
            Self::Experiential => "experiential",
            Self::Scheduled => "scheduled",
        }
    }

    /// Whether this feature is implemented yet. Flips true per feature as it
    /// lands (26.3–26.6). `tool_offload` 26.3 (#584); `scratchpad` 26.4 (#583);
    /// `semantic` 26.5 (#582); `experiential` 26.6a (#585); `scheduled` 26.6b
    /// (#586). Only `provenance` (#584, a later compaction-handle feature) is
    /// still pending.
    pub fn available(self) -> bool {
        match self {
            Self::ToolOffload
            | Self::Scratchpad
            | Self::Semantic
            | Self::Experiential
            | Self::Scheduled => true,
            Self::Provenance => false,
        }
    }

    /// The tracking issue for this feature (cited in "not yet available").
    pub fn issue(self) -> u32 {
        match self {
            Self::ToolOffload | Self::Provenance => 584,
            Self::Scratchpad => 583,
            Self::Semantic => 582,
            Self::Experiential => 585,
            Self::Scheduled => 586,
        }
    }
}

/// The resolved on/off state of every context feature (Phase 26, #588) — the
/// effective set after a `manager` preset's defaults and config/session
/// overrides are applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextFeatureSet {
    pub tool_offload: bool,
    pub scratchpad: bool,
    pub semantic: bool,
    pub provenance: bool,
    pub experiential: bool,
    pub scheduled: bool,
}

impl ContextFeatureSet {
    /// Read one feature's resolved state.
    pub fn get(&self, f: ContextFeature) -> bool {
        match f {
            ContextFeature::ToolOffload => self.tool_offload,
            ContextFeature::Scratchpad => self.scratchpad,
            ContextFeature::Semantic => self.semantic,
            ContextFeature::Provenance => self.provenance,
            ContextFeature::Experiential => self.experiential,
            ContextFeature::Scheduled => self.scheduled,
        }
    }

    /// Set one feature's resolved state.
    pub fn set(&mut self, f: ContextFeature, on: bool) {
        match f {
            ContextFeature::ToolOffload => self.tool_offload = on,
            ContextFeature::Scratchpad => self.scratchpad = on,
            ContextFeature::Semantic => self.semantic = on,
            ContextFeature::Provenance => self.provenance = on,
            ContextFeature::Experiential => self.experiential = on,
            ContextFeature::Scheduled => self.scheduled = on,
        }
    }

    /// The features currently on, in display order.
    pub fn enabled(self) -> Vec<ContextFeature> {
        ContextFeature::ALL
            .into_iter()
            .filter(|&f| self.get(f))
            .collect()
    }

    /// The base feature set *before* `[context.features]` / session overrides:
    /// the `manager` preset's bundle, with **every available context-management
    /// feature defaulted ON** (#727). The one exception is `provenance`, which
    /// is not yet implemented (`ContextFeature::available()` is `false`) and so
    /// stays OFF until it lands. This makes the full context toolkit — tool
    /// offload, the `<state>`/`<plan>` scratchpad ledger, semantic retrieval,
    /// experiential memory, and the scheduled per-step view — the default on
    /// EVERY backend rather than only local (`Ollama`) ones. Cloud endpoints
    /// (hosted `Openai`-protocol endpoints generally) can't
    /// auto-discover a context window, so they benefit most from the ledger and
    /// retrieval being on by default. Explicit overrides still win — they layer
    /// on top via [`ContextFeatures::apply_to`], so a user can turn any feature
    /// back off in `[context.features]` or with `/context feature <name> off`.
    ///
    /// `kind` is retained for signature stability and future backend-specific
    /// tuning; defaults no longer branch on it.
    pub fn base_for(manager: ContextManager, _kind: BackendKind) -> Self {
        let mut base = manager.base_features();
        for f in ContextFeature::ALL {
            if f != ContextFeature::Provenance && f.available() {
                base.set(f, true);
            }
        }
        base
    }
}

/// Per-feature overrides under `[context.features]` (config) and `/context
/// feature` (session) (Phase 26, #588). `None` inherits the `manager` preset's
/// default; `Some(b)` forces the feature on/off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFeatures {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_offload: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratchpad: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiential: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled: Option<bool>,
}

impl ContextFeatures {
    /// Read one feature's override (`None` = inherit the preset).
    pub fn get(&self, f: ContextFeature) -> Option<bool> {
        match f {
            ContextFeature::ToolOffload => self.tool_offload,
            ContextFeature::Scratchpad => self.scratchpad,
            ContextFeature::Semantic => self.semantic,
            ContextFeature::Provenance => self.provenance,
            ContextFeature::Experiential => self.experiential,
            ContextFeature::Scheduled => self.scheduled,
        }
    }

    /// Set one feature's override.
    pub fn set(&mut self, f: ContextFeature, v: Option<bool>) {
        match f {
            ContextFeature::ToolOffload => self.tool_offload = v,
            ContextFeature::Scratchpad => self.scratchpad = v,
            ContextFeature::Semantic => self.semantic = v,
            ContextFeature::Provenance => self.provenance = v,
            ContextFeature::Experiential => self.experiential = v,
            ContextFeature::Scheduled => self.scheduled = v,
        }
    }

    /// Layer these overrides onto a base set (a preset's defaults).
    pub fn apply_to(&self, mut base: ContextFeatureSet) -> ContextFeatureSet {
        for f in ContextFeature::ALL {
            if let Some(v) = self.get(f) {
                base.set(f, v);
            }
        }
        base
    }
}

/// `[context]` config section (Step 24.8, #559; features added Phase 26, #588).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextConfig {
    /// Context-management strategy preset. Default `standard`.
    #[serde(default)]
    pub manager: ContextManager,

    /// Policy for automatic compaction triggers. Default `headroom_aware`:
    /// message count alone only triggers when Newt lacks an authoritative
    /// input ceiling; token and send-budget pressure always remain active.
    #[serde(default)]
    pub compaction_trigger_policy: CompactionTriggerPolicy,

    /// Per-feature overrides under `[context.features]` — each inherits the
    /// `manager` preset's default unless explicitly set (Phase 26, #588).
    #[serde(default)]
    pub features: ContextFeatures,

    /// `[context.semantic]` — settings for the `semantic` RAG feature (Step
    /// 26.5, #582).
    #[serde(default)]
    pub semantic: SemanticConfig,

    /// `[context.estimation]` — the cheap token-estimation heuristic
    /// (`chars_per_token`, default 4). Threaded through the estimators; the
    /// per-model calibration ratio scales the result on top.
    #[serde(default)]
    pub estimation: crate::tokens::TokenEstimation,

    /// Floor (chars) for the whole-middle summarizer input cap. The cap is
    /// normally the compression budget converted to chars, but a tight budget
    /// would starve the summarizer of material — never give it less than this.
    #[serde(default = "default_summary_input_cap_floor_chars")]
    pub summary_input_cap_floor_chars: usize,

    /// `[context.api_surface]` — the workspace-API-surface knowledge_base
    /// technique + its pluggable language packs (#669).
    #[serde(default)]
    pub api_surface: ApiSurfaceConfig,

    /// Percentage bound on how much of a request's `num_ctx` window may be
    /// input before the pre-send gate trims. The effective input ceiling is
    /// the tighter of this bound and the room left by the active maximum
    /// output. The historical hardcoded value was 80. Large-window models can
    /// safely run this higher to pack more input when the output reserve is not
    /// already tighter. Normalized to `1..=99`; anything outside falls back to
    /// 80. See `num_ctx_input_ceiling` (#282).
    #[serde(
        default = "default_input_ceiling_pct",
        deserialize_with = "deserialize_input_ceiling_pct"
    )]
    pub input_ceiling_pct: u32,

    /// Percent-of-ceiling below which the loop emits the low-remaining-budget
    /// nudge to the model. Historical hardcoded value was 15. Raise it to be
    /// warned earlier, lower it to suppress the nudge on roomy models. Clamped
    /// to `0..=100`; `0` disables the nudge. See `agentic::budget` (#559).
    #[serde(default = "default_low_budget_pct")]
    pub low_budget_pct: usize,
}

fn default_summary_input_cap_floor_chars() -> usize {
    8_192
}

fn default_input_ceiling_pct() -> u32 {
    80
}

/// Normalize a configured input-ceiling percentage to its documented safe
/// domain. Invalid values fall back to the historical 80% default: zero must
/// not erase an authoritative ceiling, and 100%+ must not permit over-window
/// input budgets.
#[must_use]
pub fn normalize_input_ceiling_pct(value: u32) -> u32 {
    if (1..=99).contains(&value) {
        value
    } else {
        default_input_ceiling_pct()
    }
}

/// Resolve only the configured percentage bound for a full context window.
/// Generation-policy output reserves compose with this value in the agentic
/// loop; callers that hold an already-derived input cap must not apply it again.
#[must_use]
pub fn input_percentage_ceiling(context_window: u32, input_ceiling_pct: u32) -> u32 {
    let pct = normalize_input_ceiling_pct(input_ceiling_pct);
    (u64::from(context_window) * u64::from(pct) / 100) as u32
}

fn deserialize_input_ceiling_pct<'de, D>(deserializer: D) -> std::result::Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    u32::deserialize(deserializer).map(normalize_input_ceiling_pct)
}

fn default_low_budget_pct() -> usize {
    15
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            manager: ContextManager::default(),
            compaction_trigger_policy: CompactionTriggerPolicy::default(),
            features: ContextFeatures::default(),
            semantic: SemanticConfig::default(),
            estimation: crate::tokens::TokenEstimation::default(),
            summary_input_cap_floor_chars: default_summary_input_cap_floor_chars(),
            api_surface: ApiSurfaceConfig::default(),
            input_ceiling_pct: default_input_ceiling_pct(),
            low_budget_pct: default_low_budget_pct(),
        }
    }
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
    pub allow_shell_commands: bool,
    /// Whether the `tui-shell-commands` suite may MUTATE the filesystem
    /// (`mkdir`/`mv`, and `rm` via a recoverable graveyard). Default `false` —
    /// navigation + inspection only until the operator opts in.
    #[serde(default = "default_allow_shell_mutations")]
    pub allow_shell_mutations: bool,
}

fn default_spill_lines() -> usize {
    3
}

fn default_tool_output_lines() -> usize {
    20
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

/// A named crew (`[crews.<name>]` or `crews/<name>.toml`): a role-specialized
/// ensemble over the heterogeneous backend pool. Each role names a `[loadouts.*]`
/// (so a crew is a *composition of loadouts* — the canonical example routes the
/// planner/triage to frontier models and bulk work to cheap local inference,
/// `docs/design/config-scaling-deployment-and-trust.md`). The harness owns the
/// control loop (`run_crew`); these fields pin the workers + budgets.
///
/// ```toml
/// [crews.coder]
/// planner = "planner"          # → [loadouts.planner]  (required)
/// navigator = "navigator"      # → [loadouts.navigator]
/// triage = "triage"            # → [loadouts.triage]
/// loop = "patch-revise"        # control program (default)
///   [crews.coder.budgets]
///   max_attempts = 4
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Crew {
    /// Planner/editor role — must name a `[loadouts.<name>]`. Required (a crew
    /// with no planner can't make edits).
    pub planner: String,
    /// Repo-navigator role — names a `[loadouts.<name>]`. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigator: Option<String>,
    /// Test-triage role — names a `[loadouts.<name>]`. Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage: Option<String>,
    /// Control program (e.g. `"patch-revise"`). Omitted ⇒ the default loop.
    #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
    pub loop_program: Option<String>,
    /// Per-role dispatch wall-clock bound, seconds (#698). Omitted ⇒ the
    /// env/default (`NEWT_ROLE_TIMEOUT_SECS` → 600s). Widen it here for a slow
    /// loadout instead of relying on the env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_timeout_secs: Option<u64>,
    /// Verification command override (e.g. `"just check"`). Omitted ⇒ inferred
    /// from the repo (justfile → `just check`, Cargo → `cargo test`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    /// Budgets / safety gates for the control loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budgets: Option<CrewBudgets>,
}

/// `[crew]` dispatch policy (#749 step 2): the operator's structural tightening
/// point for crews/teams the overseer fields.
///
/// A model that fields a crew is the recursion / Confused-Deputy case. Dispatch
/// hands each crew `session ⊓ clamp` (the [`crate::Caveats`] meet), so the crew's
/// authority is **always `≤ session`** (the overseer cannot escalate by
/// dispatching) and **`≤ clamp`** (the operator's bound). With the default
/// `clamp = Caveats::top()` the meet is the identity — today's behavior is
/// unchanged — while the seam exists for tighter clamps (and the per-subtask
/// `team_clamp`, #749 step 8) to plug into.
///
/// ```toml
/// [crew]
/// # crews may reach only this host, even if the session's net grant is wider
/// [crew.clamp]
/// net = { only = ["registry.internal"] }
/// ```
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CrewPolicyConfig {
    /// The authority **clamp** dispatched crews are met against
    /// (`child = session ⊓ clamp`). Defaults to `Caveats::top()` (identity meet —
    /// behavior unchanged). Tighten an axis here to bound every crew below the
    /// session ceiling; later steps (#749 step 8) compose a per-subtask clamp on
    /// top of this at the same `dispatch` seam.
    #[serde(default)]
    pub clamp: crate::caveats::Caveats,
}

/// Budgets + review gates for a crew's control loop (`crew-loadout.md` §budgets).
/// Consumed by the front door; an honest cap-exit at `max_attempts` returns
/// `NeedsHumanReview`, never a false success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CrewBudgets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_files_touched: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lines_changed: Option<u32>,
    /// Topics that force a human-review pause (e.g. `["auth","crypto","migrations"]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_human_review_on: Vec<String>,
}

impl Crew {
    /// Validate the crew's role references against `cfg`: each named role
    /// (`planner`/`navigator`/`triage`) must name a known `[loadouts.<name>]`,
    /// and that loadout must itself validate (so a crew transitively checks the
    /// whole `crew → loadout → {backend,bundle,profile}` chain). A dangling role
    /// is a hard error — a crew that silently dropped a worker would be a false
    /// claim.
    ///
    /// # Errors
    /// The first dangling or invalid role reference, as a message.
    pub fn validate(&self, cfg: &Config) -> std::result::Result<(), String> {
        let check = |label: &str, name: &str| -> std::result::Result<(), String> {
            let loadout = cfg.loadouts.get(name).ok_or_else(|| {
                let known = if cfg.loadouts.is_empty() {
                    "none defined".to_string()
                } else {
                    cfg.loadouts.keys().cloned().collect::<Vec<_>>().join(", ")
                };
                format!(
                    "crew {label} '{name}': no [loadouts] entry named '{name}' (known: {known})"
                )
            })?;
            loadout
                .validate(cfg)
                .map_err(|e| format!("crew {label} '{name}': {e}"))
        };
        check("planner", &self.planner)?;
        if let Some(nav) = &self.navigator {
            check("navigator", nav)?;
        }
        if let Some(tri) = &self.triage {
            check("triage", tri)?;
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
    /// Readline / emacs-style bindings.
    Emacs,
    /// Vi / vim-style bindings — Esc for normal mode, i for insert.
    Vi,
    /// Nano-style: modeless, emacs-like bindings (the **default** — the most
    /// broadly approachable). Behaves like `Emacs` on the lean surface; it is a
    /// distinct, selectable label, and the rich-tui surface shows the nano `^G`
    /// help hint for it.
    #[default]
    Nano,
}

impl EditMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Emacs => "emacs",
            Self::Vi => "vi",
            Self::Nano => "nano",
        }
    }

    /// Cycle through the modes (used by a single-key toggle): emacs → vi →
    /// nano → emacs.
    pub fn toggle(&self) -> Self {
        match self {
            Self::Emacs => Self::Vi,
            Self::Vi => Self::Nano,
            Self::Nano => Self::Emacs,
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            chat_style: ChatStyle::Compact,
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
    /// An **in-process** inference backend — no HTTP, no external server. Loads a
    /// small quantized (GGUF) model and runs it in-tree (Metal-accelerated on
    /// Apple Silicon). Opt-in behind the `embedded` cargo feature (default-off);
    /// when the feature is absent, selecting it is a clear build-time-off error,
    /// never a silent fallback. Intended for the summarizer + small auxiliary
    /// calls so they never contend with the primary model (#639).
    Embedded,
    /// Anthropic's native Messages API (`POST /v1/messages`, `GET /v1/models`),
    /// authenticated with `x-api-key` + `anthropic-version` headers (NOT a
    /// bearer token). A genuinely distinct wire: top-level `system`, required
    /// `max_tokens`, content-block responses. Unlike llama.cpp/vLLM (which
    /// share the OpenAI wire and are told apart by [`Engine`] metadata),
    /// Anthropic earns its own kind because the protocol differs.
    #[serde(alias = "claude")]
    Anthropic,
}

impl BackendKind {
    /// Short human label for the wire protocol — shown in the ready preamble and
    /// the `/backends` list. Note newt models the *protocol*, so vLLM, llama.cpp,
    /// and hosted OpenAI all read as `openai` (vLLM has no distinct wire form).
    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::Openai => "openai",
            Self::Embedded => "embedded",
            Self::Anthropic => "anthropic",
        }
    }
}

/// The inference ENGINE behind an endpoint — pure metadata, orthogonal to
/// [`BackendKind`] (the wire protocol). llama.cpp's server and vLLM both
/// speak the OpenAI wire, so `kind` alone cannot tell them apart; a
/// fingerprint probe (`backend_probe::detect_engine`) can. The engine never
/// gates a transport — it drives only which warm-model probe applies, display
/// labels, and future model-card hints. `None` = undetected/unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// Ollama (`/api/version`, `/api/tags`, `/api/ps`).
    Ollama,
    /// llama.cpp's `llama-server` (`/props`, non-`/v1` `/models` with load
    /// states).
    #[serde(alias = "llama-cpp", alias = "llama.cpp")]
    LlamaCpp,
    /// vLLM (`/version`, single served model per instance).
    Vllm,
}

impl Engine {
    /// Short human label — shown beside probe results and in `/backends`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ollama => "ollama",
            Self::LlamaCpp => "llama.cpp",
            Self::Vllm => "vllm",
        }
    }
}

/// Which OpenAI HTTP surface a `kind = "openai"` backend speaks.
///
/// `chat_completions` (the default) is the classic `POST /v1/chat/completions`.
/// `responses` is the newer `POST /v1/responses` — required by models that
/// OpenAI serves *only* there (e.g. `gpt-5-codex`, which 404s on
/// chat/completions with "only supported in v1/responses"). Ignored for
/// `kind = "ollama"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiApi {
    /// `POST /v1/chat/completions` (the historical default).
    #[default]
    #[serde(alias = "chat", alias = "completions")]
    ChatCompletions,
    /// `POST /v1/responses` (the newer Responses API).
    Responses,
}

impl OpenAiApi {
    /// Short human label for the HTTP surface.
    pub fn label(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }
}

/// A single inference backend entry.
///
/// Two ways to define one: an inline `[[backends]]` array element in
/// `config.toml`, or a per-file drop-in `~/.newt/backends/<name>.toml` (the
/// How a backend SERVES models — orthogonal to [`BackendKind`] (the wire
/// protocol). The out-of-the-box epic's (#1126) second axis:
///
/// - **Multiplexer** (Ollama; also an OpenAI-compatible gateway fronting many
///   models): many models, the client picks per request (`/model` swaps
///   freely), capabilities are learned **per model**.
/// - **Instance** (vLLM; the embedded engine): bound to ONE base model at
///   startup — `/v1/models` exists only to *declare* it. newt ADOPTS the
///   served model; capabilities attach to the **backend**; `/model` reports
///   "fixed — restart the server or `/backends` to switch".
///
/// Usually left unset in the file and DERIVED by probing (see
/// [`derive_serving`]), then cached back as provenance by `newt setup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Serving {
    Multiplexer,
    Instance,
}

/// Whether newt actively **tends** this backend's host — a shared, model-
/// swapping box (e.g. a llama.cpp router) — rather than merely consuming a
/// dedicated endpoint. Orthogonal to [`BackendKind`] (the wire) and
/// [`Serving`] (how the box serves). See ADR `docs/decisions/managed_backend.md`.
///
/// - **`Shared`** — cooperative guest: the box may serve other consumers
///   (including other newt-agents), so the default is to **adopt whatever model
///   is warm** rather than force a swap (see [`crate::backend_probe::adopt`]).
///   This is the clash-avoidance primitive — two agents on one box don't thrash
///   the single-model swap.
/// - **`Dedicated`** — "I own this box": newt may force its configured model
///   (force-load + keep-warm are later slices). No adopt-warm.
///
/// Unset on a backend = an ordinary consumed endpoint (no swap-awareness).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedMode {
    Shared,
    Dedicated,
}

/// Derive the serving axis from what a probe saw: the wire kind plus how many
/// models the endpoint reported. Pure — Phase B's probe/adopt calls this; kept
/// here so the rule lives beside the type. `served_count` = models listed by
/// `/api/tags` (ollama) or `/v1/models` (openai).
pub fn derive_serving(kind: BackendKind, served_count: usize) -> Serving {
    match kind {
        // Ollama loads models on demand — always a multiplexer, even if only
        // one model happens to be pulled today.
        BackendKind::Ollama => Serving::Multiplexer,
        // A vLLM instance declares exactly one model; an OpenAI-compatible
        // gateway fronting a fleet lists many.
        BackendKind::Openai => {
            if served_count == 1 {
                Serving::Instance
            } else {
                Serving::Multiplexer
            }
        }
        // The in-process engine runs one GGUF.
        BackendKind::Embedded => Serving::Instance,
        // A hosted API fronting the whole Claude family — always many models.
        BackendKind::Anthropic => Serving::Multiplexer,
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

/// Where a backend file came from — written by `newt setup`, hand-authored,
/// or probe-derived. Pure data; nothing branches on it. Makes a generated
/// file self-describing and lets `doctor` show declared-vs-derived drift.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackendProvenance {
    /// Who wrote the file (e.g. `newt setup v0.7.3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// When the endpoint was last probed (ISO 8601 date or datetime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probed: Option<String>,
    /// True when `serving` was derived by the probe rather than hand-declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_serving: Option<bool>,
}

/// filename stem is the `name`, so a drop-in omits it). `name` and `tiers`
/// therefore default — a minimal drop-in is just `endpoint` + `model`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendConfig {
    /// Backend name. For a per-file drop-in this is overwritten by the filename
    /// stem, so the file body may omit it.
    #[serde(default)]
    pub name: String,
    /// HTTP endpoint URL (Ollama / OpenAI). Defaulted so a `kind = "embedded"`
    /// backend — which runs in-process and has no URL — can omit it.
    #[serde(default)]
    pub endpoint: String,
    /// The model this backend serves. OPTIONAL (#1128, epic #1126): an unset
    /// model means "the server dictates" — Phase B's probe/adopt fills it in at
    /// session start. Configs that set it keep exactly today's behavior; read
    /// through [`effective_model`](Self::effective_model), never directly, so a
    /// `None` can never misroute a request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// For `kind = "embedded"`: the local GGUF model file (the in-process engine
    /// has no `endpoint`). `~/` is expanded at use. Ignored for HTTP backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_path: Option<String>,
    #[serde(default)]
    pub tiers: Vec<Tier>,
    /// Which wire protocol this backend speaks. OPTIONAL (#backend-kind-probe):
    /// unset means "probe at connect" via [`crate::backend_probe::detect_endpoint`]
    /// (race `/api/tags` vs `/v1/models`). Explicit `kind = "ollama"|"openai"|…`
    /// keeps today's pinned behavior. Auth stays explicit (`api_key_*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<BackendKind>,
    /// For `kind = "openai"`: which OpenAI HTTP surface to use
    /// (`chat_completions` or `responses`). OPTIONAL: unset means probe at
    /// connect (try chat/completions; adopt `responses` when the server says
    /// the model is responses-only). Explicit values stay pinned. Ignored for
    /// Ollama. Serialized only when set so a minimal drop-in stays minimal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<OpenAiApi>,
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
    /// The serving axis (multiplexer | instance) — see [`Serving`]. Unset =
    /// derive by probing (Phase B); `newt setup` caches the derivation here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving: Option<Serving>,
    /// When set, newt actively **tends** this backend's host rather than merely
    /// consuming it — see [`ManagedMode`] and ADR
    /// `docs/decisions/managed_backend.md`. `Shared` makes
    /// [`crate::backend_probe::adopt`] prefer a warm model over forcing a swap
    /// (clash-avoidance for several agents on one box); unset = an ordinary
    /// consumed endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed: Option<ManagedMode>,
    /// The detected inference engine (ollama | llama.cpp | vllm) — see
    /// [`Engine`]. Pure metadata, orthogonal to `kind`: never gates a
    /// transport, only refines warm-model probing and display. Unset =
    /// undetected; `newt setup` caches the fingerprint result here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<Engine>,
    /// The physical host this endpoint lives on, for same-host reasoning (the
    /// vLLM-starves-ollama rule, crew spread). Unset = derived from the
    /// endpoint URL's host part; set it only to group endpoints the URL
    /// doesn't reveal as co-located.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Big-box escape hatch: `true` asserts this host has room to run this
    /// backend ALONGSIDE others (e.g. a huge-RAM ollama next to a small vLLM),
    /// suppressing the default "vLLM resident ⇒ same-host ollama is starved"
    /// rule. Unset = the conservative default applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coexist: Option<bool>,
    /// Host memory available for serving (GiB), for the crew fit-gate
    /// (Σ model `footprint_gib` ≤ `ram_gib`). Unset = unknown (fit-gate
    /// falls back to the conservative one-model law).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ram_gib: Option<f64>,
    /// Model-card pointer: the card whose serving/tuning/capability blocks
    /// apply to this backend's model (instance backends especially).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    /// Inline capability overrides for THIS backend — same shape as a model
    /// card's `[capability]` (reused type). On an instance backend this is
    /// where adopted capabilities live; a multiplexer keeps per-model
    /// capabilities in the probe cache instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<crate::model_card::Capability>,
    /// Self-description of how this file came to be — see
    /// [`BackendProvenance`]. Written by `newt setup`; never read at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<BackendProvenance>,
}

impl BackendConfig {
    /// Resolve explicitly accepted Chat Completions request extensions.
    #[must_use]
    pub fn chat_completions_capability(&self) -> crate::model_card::ChatCompletionsCapability {
        self.capability
            .as_ref()
            .and_then(|capability| capability.chat_completions)
            .unwrap_or_default()
    }

    /// Resolve the backend's reasoning replay contract. Unknown or legacy
    /// endpoints remain conservative and never receive replayed reasoning.
    #[must_use]
    pub fn reasoning_replay_scope(&self) -> crate::model_card::ReasoningReplayScope {
        self.capability
            .as_ref()
            .and_then(|capability| capability.reasoning_replay_scope)
            .unwrap_or_default()
    }

    /// The declared model, if any — empty strings count as unset. This is the
    /// ONLY sanctioned way to read `model`; when it returns `None` the backend
    /// expects the served model to be adopted from the endpoint (Phase B).
    pub fn effective_model(&self) -> Option<&str> {
        self.model.as_deref().filter(|m| !m.trim().is_empty())
    }

    /// True when `kind` was omitted — session start / doctor must run
    /// [`crate::backend_probe::detect_endpoint`] before speaking the wire.
    pub fn needs_kind_probe(&self) -> bool {
        self.kind.is_none()
    }

    /// Human label for lists/preambles: the pinned protocol, or `"auto"` when
    /// unset (probe fills it in at connect).
    pub fn kind_label(&self) -> &'static str {
        self.kind.map(BackendKind::label).unwrap_or("auto")
    }

    /// Resolve this backend's bearer token, if any.
    ///
    /// Checks [`api_key_env`](Self::api_key_env) first (environment
    /// variable), then [`api_key_file`](Self::api_key_file) — plaintext
    /// (first non-empty line, trimmed) or age-encrypted (`.token.age`,
    /// decrypted through [`crate::secrets`]). Returns `None` when nothing
    /// resolves; a LOCKED/broken encrypted token additionally warns once per
    /// path so it is never a silent `None` (use
    /// [`resolve_api_key_detailed`](Self::resolve_api_key_detailed) for the
    /// typed reason).
    pub fn resolve_api_key(&self) -> Option<String> {
        match self.resolve_api_key_detailed() {
            Ok(v) => v,
            Err(e) => {
                crate::secrets::warn_once(self.api_key_file.as_deref().unwrap_or(&self.name), &e);
                None
            }
        }
    }

    /// [`resolve_api_key`](Self::resolve_api_key) with the typed failure —
    /// doctor and worker startup lines surface the actionable reason
    /// (passphrase required / wrong passphrase / corrupt file).
    pub fn resolve_api_key_detailed(
        &self,
    ) -> std::result::Result<Option<String>, crate::secrets::SecretsError> {
        resolve_api_key_common(self.api_key_env.as_deref(), self.api_key_file.as_deref())
    }
}

/// The ONE env-then-file credential rule shared by [`BackendConfig`] and
/// [`SummarizerConfig`]. Env wins when set and non-empty; the file path goes
/// through `secrets::resolve_token_file` (plaintext and encrypted alike).
pub(crate) fn resolve_api_key_common(
    api_key_env: Option<&str>,
    api_key_file: Option<&str>,
) -> std::result::Result<Option<String>, crate::secrets::SecretsError> {
    if let Some(var) = api_key_env {
        if let Ok(val) = std::env::var(var) {
            let val = val.trim();
            if !val.is_empty() {
                return Ok(Some(val.to_string()));
            }
        }
    }
    if let Some(path) = api_key_file {
        let expanded = expand_tilde(path);
        return crate::secrets::resolve_token_file(&expanded);
    }
    Ok(None)
}

/// CLI-supplied backend override (`newt --backend-*` flags). Each field mirrors
/// an operator-settable [`BackendConfig`] field; `None` means "not set on the
/// command line". Applied LAST in [`Config::resolve`] so it wins over disk
/// drop-ins and localhost discovery — the explicit, per-invocation escape hatch
/// for "use EXACTLY this backend", which no probe write-back or auto-discovery
/// can then override. Set once from the CLI via [`set_cli_backend_override`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BackendOverride {
    /// Backend name (default `"cli"`). Names the exclusive backend, or selects
    /// which existing backend a field-only override targets.
    pub name: Option<String>,
    pub endpoint: Option<String>,
    pub model: Option<String>,
    pub model_path: Option<String>,
    pub tiers: Option<Vec<Tier>>,
    pub kind: Option<BackendKind>,
    pub api: Option<OpenAiApi>,
    pub api_key_env: Option<String>,
    pub api_key_file: Option<String>,
    pub serving: Option<Serving>,
    pub engine: Option<Engine>,
    pub host: Option<String>,
    pub coexist: Option<bool>,
    pub ram_gib: Option<f64>,
    pub card: Option<String>,
}

impl BackendOverride {
    /// True when no `--backend-*` flag was set (the common case) — [`apply`] is
    /// then a no-op.
    ///
    /// [`apply`]: Self::apply
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Apply to a resolved config.
    ///
    /// When a destination is given (`endpoint` or `model_path`), the override
    /// defines an **exclusive** backend that REPLACES all others — so CLI intent
    /// beats disk drop-ins and localhost discovery, and no later probe write-back
    /// can misroute the session. Tiers default to all four when unset so the
    /// backend actually serves. Without a destination the provided fields
    /// **override in place** the backend named by `name` (else the first
    /// backend), e.g. `--backend-model` to swap only the model.
    pub fn apply(&self, cfg: &mut Config) {
        if self.is_empty() {
            return;
        }
        // An explicit `--backend-*` flag is operator configuration — the
        // session is no longer running on the bare compiled-in fallback.
        cfg.backend_fallback = false;
        let name = self.name.clone().unwrap_or_else(|| "cli".to_string());
        let has_destination = self.endpoint.is_some() || self.model_path.is_some();

        if has_destination {
            let mut backend = cfg
                .backends
                .iter()
                .find(|b| b.name == name)
                .cloned()
                .unwrap_or_else(|| BackendConfig {
                    name: name.clone(),
                    ..Default::default()
                });
            backend.name = name;
            self.overlay(&mut backend);
            if backend.tiers.is_empty() {
                backend.tiers = vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review];
            }
            cfg.backends = vec![backend];
            return;
        }

        // Field-only override: mutate the named backend (else the first) in place.
        let idx = match self.name.as_deref() {
            Some(n) => cfg.backends.iter().position(|b| b.name == n),
            None => (!cfg.backends.is_empty()).then_some(0),
        };
        if let Some(i) = idx {
            self.overlay(&mut cfg.backends[i]);
        }
    }

    /// Copy every set field onto `backend` (leaving unset fields untouched).
    fn overlay(&self, backend: &mut BackendConfig) {
        if let Some(v) = &self.endpoint {
            backend.endpoint = v.clone();
        }
        if let Some(v) = &self.model {
            backend.model = Some(v.clone());
        }
        if let Some(v) = &self.model_path {
            backend.model_path = Some(v.clone());
        }
        if let Some(v) = &self.tiers {
            backend.tiers = v.clone();
        }
        if let Some(v) = self.kind {
            backend.kind = Some(v);
        }
        if let Some(v) = self.api {
            backend.api = Some(v);
        }
        if let Some(v) = &self.api_key_env {
            backend.api_key_env = Some(v.clone());
        }
        if let Some(v) = &self.api_key_file {
            backend.api_key_file = Some(v.clone());
        }
        if let Some(v) = self.serving {
            backend.serving = Some(v);
        }
        if let Some(v) = self.engine {
            backend.engine = Some(v);
        }
        if let Some(v) = &self.host {
            backend.host = Some(v.clone());
        }
        if let Some(v) = self.coexist {
            backend.coexist = Some(v);
        }
        if let Some(v) = self.ram_gib {
            backend.ram_gib = Some(v);
        }
        if let Some(v) = &self.card {
            backend.card = Some(v.clone());
        }
    }
}

/// Process-global CLI backend override, set once from the CLI before any config
/// application. Mirrors the other publishes in
/// [`Config::apply_runtime_settings`] (max_output_tokens, scratch dir): the CLI
/// can't thread a value through every runtime consumer, so it stashes it here
/// and the canonical apply operation installs it last.
static CLI_BACKEND_OVERRIDE: std::sync::Mutex<Option<BackendOverride>> =
    std::sync::Mutex::new(None);

/// Install the CLI backend override (see [`BackendOverride`]). Call once, before
/// the first [`Config::apply_runtime_settings`] call.
pub fn set_cli_backend_override(over: BackendOverride) {
    if let Ok(mut slot) = CLI_BACKEND_OVERRIDE.lock() {
        *slot = Some(over);
    }
}

fn cli_backend_override() -> Option<BackendOverride> {
    CLI_BACKEND_OVERRIDE.lock().ok().and_then(|s| s.clone())
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
    /// Nothing explicitly selected and nothing configured qualified.
    Unset,
}

// ---------------------------------------------------------------------------
// Default
// ---------------------------------------------------------------------------

/// Write one backend as a per-file drop-in `<config dir>/backends/<name>.toml`
/// (#1140, epic #1126) — the shape `merge_backends_from_dir` reads back. The
/// canonical writer for `newt init` / `newt setup`: one endpoint, one file,
/// provenance-stamped by the caller. Returns the written path.
pub fn write_backend_dropin(
    config_path: &std::path::Path,
    backend: &BackendConfig,
) -> std::result::Result<std::path::PathBuf, String> {
    if backend.name.trim().is_empty() {
        return Err("backend drop-in needs a name (it becomes the filename)".into());
    }
    let dir = config_path.with_file_name("backends");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.toml", backend.name));
    let body = toml::to_string(backend).map_err(|e| format!("serialize backend: {e}"))?;
    std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Persist probed backend fields into `~/.newt/backends/<name>.toml` (or
/// `$NEWT_CONFIG_DIR/backends/<name>.toml`) — never into the main
/// `config.toml`. Reset = delete that one file.
///
/// Merges into an existing drop-in of the same name (preserving auth refs and
/// any operator-set fields not in `patch`), else creates a new minimal drop-in
/// from `patch`. Returns the written path, or `None` when there is no user
/// config dir / empty name.
pub fn writeback_probed_backend(
    patch: &BackendConfig,
) -> std::result::Result<Option<std::path::PathBuf>, String> {
    if patch.name.trim().is_empty() {
        return Ok(None);
    }
    let Some(config_path) = Config::user_config_path() else {
        return Ok(None);
    };
    let dir = config_path.with_file_name("backends");
    let path = dir.join(format!("{}.toml", patch.name));
    let mut merged = if path.is_file() {
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        toml::from_str::<BackendConfig>(&text)
            .map_err(|e| format!("parse {}: {e}", path.display()))?
    } else {
        BackendConfig {
            name: patch.name.clone(),
            endpoint: patch.endpoint.clone(),
            ..Default::default()
        }
    };
    // Filename stem is authoritative.
    merged.name = patch.name.clone();
    if !patch.endpoint.is_empty() {
        merged.endpoint = patch.endpoint.clone();
    }
    if patch.kind.is_some() {
        merged.kind = patch.kind;
    }
    if patch.api.is_some() {
        merged.api = patch.api;
    }
    if patch.model.is_some() {
        merged.model = patch.model.clone();
    }
    if patch.serving.is_some() {
        merged.serving = patch.serving;
    }
    if patch.api_key_env.is_some() {
        merged.api_key_env = patch.api_key_env.clone();
    }
    if patch.api_key_file.is_some() {
        merged.api_key_file = patch.api_key_file.clone();
    }
    merged.provenance = Some(BackendProvenance {
        source: Some(format!(
            "newt adopt v{} (probed; delete this file to reset)",
            crate::build_info::VERSION_WITH_COMMIT
        )),
        probed: Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
        derived_serving: patch
            .serving
            .map(|_| true)
            .or_else(|| merged.provenance.as_ref().and_then(|p| p.derived_serving)),
    });
    write_backend_dropin(&config_path, &merged).map(Some)
}

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
    /// drop-in, no `--backend-*` CLI override — the backend list is exactly
    /// the compiled-in localhost fallback. This is the first-run wizard's
    /// "nothing configured" predicate: [`Config::resolve`] otherwise silently
    /// invents a localhost Ollama, so a missing config was never observable
    /// as a state. Meaningful only on a config produced by `resolve()`.
    #[must_use]
    pub fn is_unconfigured(&self) -> bool {
        self.backend_fallback
    }

    pub fn resolve() -> Result<Self> {
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
                merge_toml(&mut merged, project_val, strategy);
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
        cfg.merge_disk_backends();
        // Localhost fallback: a config that declared no inline `[[backends]]`
        // deserializes to empty (see the field doc); if no drop-in supplied one
        // either, restore the bare-install localhost Ollama so newt still has a
        // backend to talk to.
        if cfg.backends.is_empty() {
            cfg.backend_fallback = true;
            cfg.backends.push(fallback_localhost_backend());
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
        cfg.apply_runtime_settings();
        Ok(cfg)
    }

    /// Apply one final resolved configuration to the runtime.
    ///
    /// Configuration loading stays pure. Runtime consumers that use
    /// [`Config::load`] for an explicit profile must invoke this once after
    /// loading; normal discovery via [`Config::resolve`] invokes it before
    /// returning. This is also the single owner for process-global
    /// `--backend-*` precedence, so an explicit config file cannot defeat a
    /// higher-precedence per-invocation backend pin.
    pub fn apply_runtime_settings(&mut self) {
        // CLI `--backend-*` flags win over every configuration source. Apply
        // them here, after both explicit loading and normal discovery have
        // finished, so all runtime entry points receive the same backend.
        if let Some(over) = cli_backend_override() {
            over.apply(self);
        }
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

    /// Merge per-file backends from the `backends/` dirs next to the config:
    /// `~/.newt/backends/*.toml` first, then the project `.newt/backends/` (so
    /// project overrides home overrides inline `[[backends]]`). Filename stem =
    /// backend name. A malformed drop-in is skipped with a warning; it must not
    /// break startup.
    fn merge_disk_backends(&mut self) {
        if let Some(dir) = Self::user_config_dir() {
            self.merge_backends_from_dir(&dir.join("backends"));
        }
        if let Some(proj) = Self::project_config_path() {
            if let Some(parent) = proj.parent() {
                self.merge_backends_from_dir(&parent.join("backends"));
            }
        }
    }

    /// Load `<dir>/*.toml` as backends (filename stem = name) into
    /// `self.backends`. A drop-in **replaces** an existing backend of the same
    /// name (last-wins), else it is appended — so a `dgx1.toml` file supersedes
    /// an inline `[[backends]]` named `dgx1` without a duplicate. A malformed
    /// file is skipped with a warning.
    fn merge_backends_from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // no backends dir — fine
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
            match std::fs::read_to_string(&path).map(|t| toml::from_str::<BackendConfig>(&t)) {
                Ok(Ok(mut backend)) => {
                    // A backend needs a destination: an HTTP `endpoint`, or — for
                    // `kind = "embedded"` — a local `model_path`. Skip those with
                    // neither (the "malformed → skip, not fatal" contract; before
                    // `endpoint` became defaultable, the missing-endpoint case was
                    // a parse error).
                    if backend.endpoint.is_empty() && backend.model_path.is_none() {
                        tracing::warn!(
                            path = %path.display(),
                            "skipping backend with neither endpoint nor model_path"
                        );
                        continue;
                    }
                    // The filename is authoritative for the name (collision-free).
                    backend.name = stem.to_string();
                    // A successfully merged drop-in is operator-supplied
                    // configuration — the resolved backend list is no longer
                    // the bare compiled-in fallback (see `is_unconfigured`).
                    self.backend_fallback = false;
                    match self.backends.iter_mut().find(|b| b.name == backend.name) {
                        Some(existing) => {
                            // A probe-cache drop-in records probed REALITY
                            // (endpoint / model / api / serving) — it must never
                            // CLEAR auth the config declared. Preserve api_key_*
                            // when the drop-in omits them; otherwise an
                            // OpenAI-kind backend silently loses its bearer token
                            // after the first adopt writeback (the writeback
                            // never persists secrets), and every later session
                            // 401s. See writeback_probed_backend.
                            if backend.api_key_env.is_none() {
                                backend.api_key_env = existing.api_key_env.clone();
                            }
                            if backend.api_key_file.is_none() {
                                backend.api_key_file = existing.api_key_file.clone();
                            }
                            // Same contract for tier assignment: probing records
                            // reality (endpoint / model / api / serving) but never
                            // tiers — tiers are an operator choice, so the adopt
                            // writeback always emits `tiers = []`. An empty
                            // drop-in `tiers` must not CLEAR the tiers the config
                            // declared, or the backend serves no tier after the
                            // first writeback and newt silently falls back to an
                            // auto-discovered local backend.
                            if backend.tiers.is_empty() {
                                backend.tiers = existing.tiers.clone();
                            }
                            *existing = backend;
                        }
                        None => self.backends.push(backend),
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping malformed backend file");
                }
                Err(_) => {}
            }
        }
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

    /// Merge per-file crews from the `crews/` dirs next to the config:
    /// `~/.newt/crews/*.toml` first, then the project `.newt/crews/` (so project
    /// overrides home overrides inline `[crews.*]`). Filename stem = crew name. A
    /// malformed drop-in is skipped with a warning; references inside a crew are
    /// validated when it is selected (`newt crew --crew <name>`), mirroring the
    /// inline `[crews.*]` and disk-loadout paths.
    fn merge_disk_crews(&mut self) {
        if let Some(dir) = Self::user_config_dir() {
            self.merge_crews_from_dir(&dir.join("crews"));
        }
        if let Some(proj) = Self::project_config_path() {
            if let Some(parent) = proj.parent() {
                self.merge_crews_from_dir(&parent.join("crews"));
            }
        }
    }

    /// Load `<dir>/*.toml` as crews (filename stem = name) into `self.crews`,
    /// last-wins on a name clash. A malformed file is skipped with a warning.
    fn merge_crews_from_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return; // no crews dir — fine
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
            match std::fs::read_to_string(&path).map(|t| toml::from_str::<Crew>(&t)) {
                Ok(Ok(crew)) => {
                    self.crews.insert(stem.to_string(), crew);
                }
                Ok(Err(e)) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping malformed crew file");
                }
                Err(_) => {}
            }
        }
    }

    /// Merge per-file bundles from the well-known `bundles/` dirs next to the
    /// config: `~/.newt/bundles/*.toml` first, then the project `.newt/bundles/`
    /// (so project overrides home overrides inline `[bundles.*]`). The filename
    /// stem is the bundle name. A malformed drop-in is skipped with a warning — it
    /// must not break startup.
    fn merge_disk_bundles(&mut self) {
        if let Some(dir) = Self::user_config_dir() {
            self.merge_bundles_from_dir(&dir.join("bundles"));
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
        if let Some(dir) = Self::user_config_dir() {
            self.merge_loadouts_from_dir(&dir.join("loadouts"));
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

    /// The user-writable config root: `$NEWT_CONFIG_DIR` or `~/.newt`.
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
    /// names a backend > `default_backend` (usable) > a sole backend > prefer an
    /// OpenAI-kind entry, else the first endpoint-bearing one. `None` only when no
    /// backend has an endpoint. Env-synthesized fallbacks (codex, legacy dgx,
    /// localhost) stay in chat's `resolve_backend_choice`, layered around this.
    #[must_use]
    pub fn select_configured_backend(&self) -> Option<&BackendConfig> {
        // 1. Operator / live override: $NEWT_PROVIDER names a backend.
        if let Ok(name) = std::env::var("NEWT_PROVIDER") {
            if !name.is_empty() {
                if let Some(b) = self.backends.iter().find(|b| b.name == name) {
                    return Some(b);
                }
            }
        }
        // 2. The configured default (usable — skip an endpointless one).
        if let Some(name) = &self.default_backend {
            if let Some(b) = self
                .backends
                .iter()
                .find(|b| b.name == *name && !b.endpoint.is_empty())
            {
                return Some(b);
            }
        }
        // 3. A sole backend is the obvious choice.
        if self.backends.len() == 1 {
            return self.backends.first().filter(|b| !b.endpoint.is_empty());
        }
        // 4. Prefer an OpenAI-kind entry, else the first endpoint-bearing one.
        self.backends
            .iter()
            .find(|b| b.kind == Some(BackendKind::Openai) && !b.endpoint.is_empty())
            .or_else(|| self.backends.iter().find(|b| !b.endpoint.is_empty()))
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
    /// caller must NOT fall back to a different backend); `Unset` only when
    /// nothing is explicitly selected and nothing configured qualifies, at which
    /// point the caller may fall back to local discovery.
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
            // A usable backend claims this name → fall through to the shared
            // precedence below (which re-checks `$NEWT_PROVIDER` / `default_backend`
            // and selects exactly that backend). Backends win a name tie.
            let usable_backend = self
                .backends
                .iter()
                .any(|b| b.name == name && !b.endpoint.is_empty());
            if !usable_backend {
                // A provider claims this name → select it.
                if let Some(provider) = self.providers.iter().find(|p| p.name == name) {
                    return SelectionOutcome::Selected(SelectedBackend::Provider(provider));
                }
                // The name matches nothing configured — neither a backend (even an
                // endpointless one) nor a provider — so it is an operator error.
                // (A name matching only an endpointless backend is "configured but
                // unusable", not "unknown": fall through to the preference rules.)
                if !self.backends.iter().any(|b| b.name == name) {
                    return SelectionOutcome::UnknownNamed(name);
                }
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
        let text = toml::to_string_pretty(self).map_err(|e| NewtError::Config(e.to_string()))?;
        std::fs::write(path, text).map_err(NewtError::Io)
    }

    /// The confined leash MCP *probe* children run under — shared by
    /// `newt doctor` and `newt mcp probe` (#1292): the operator's configured
    /// `[tui]` permissions preset, or a **ReadOnly, no-prompt default** when
    /// none is configured — the session's "safe by default, never `top()`"
    /// rule (#94). The spawn path widens exec by exactly the probed command
    /// (`newt-mcp-client`'s `spawn_caveats`); everything else stays closed.
    #[must_use]
    pub fn mcp_probe_caveats(&self, workspace: &Path) -> crate::caveats::Caveats {
        let ws = workspace.to_string_lossy();
        self.tui
            .as_ref()
            .map(|t| t.permissions.to_caveats(&ws))
            .unwrap_or_else(|| {
                ToolPermissions {
                    preset: PermissionPreset::ReadOnly,
                    extra_exec: Vec::new(),
                    net: Vec::new(),
                    prompt: false,
                }
                .to_caveats(&ws)
            })
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

    /// #904: append `host` to `[tui.permissions] net` in the TOML `text`,
    /// **preserving comments and formatting** — unlike [`Config::save`], which
    /// re-serializes the whole typed struct and drops the user's comments,
    /// ordering, and any keys newt does not model. Creates the
    /// `[tui.permissions]` table and its `net` array if absent; a no-op if the
    /// host is already listed. PURE (no I/O), so it unit-tests without a
    /// filesystem. This is the durable "allow permanently" grant path — it is
    /// only ever driven by an explicit human keypress at the permission prompt.
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

    pub fn with_net_host(text: &str, host: &str) -> Result<String> {
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| NewtError::Config(format!("config is not valid TOML: {e}")))?;
        let tui = doc
            .as_table_mut()
            .entry("tui")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        let tui_tbl = tui
            .as_table_mut()
            .ok_or_else(|| NewtError::Config("[tui] is not a table".to_string()))?;
        let perms = tui_tbl
            .entry("permissions")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        let perms_tbl = perms
            .as_table_mut()
            .ok_or_else(|| NewtError::Config("[tui.permissions] is not a table".to_string()))?;
        let net =
            perms_tbl
                .entry("net")
                .or_insert(toml_edit::Item::Value(toml_edit::Value::Array(
                    toml_edit::Array::new(),
                )));
        let arr = net.as_array_mut().ok_or_else(|| {
            NewtError::Config("[tui.permissions] net is not an array".to_string())
        })?;
        if !arr.iter().any(|v| v.as_str() == Some(host)) {
            arr.push(host);
        }
        Ok(doc.to_string())
    }

    /// Durably grant a net host by appending it to `[tui.permissions] net` in the
    /// config file at `path`, comment-preserving (see [`Config::with_net_host`]).
    /// A missing file is treated as empty (the table is created). Creates parent
    /// dirs as needed. Used by the interactive gate's "allow permanently" choice.
    pub fn append_permission_net_host(path: &Path, host: &str) -> Result<()> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let updated = Self::with_net_host(&text, host)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(NewtError::Io)?;
        }
        std::fs::write(path, updated).map_err(NewtError::Io)
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
// Helpers
// ---------------------------------------------------------------------------

/// Query-param keys (case-insensitive) whose values are treated as secrets when
/// redacting an MCP `url` for an audit dump ([`Config::to_redacted_toml`], #1301).
const SENSITIVE_QUERY_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "token",
    "access_token",
    "secret",
    "password",
    "passphrase",
    "key",
];

/// CLI flags (case-insensitive) whose value is a secret when redacting MCP
/// `args` — both the `--flag=VALUE` and `--flag VALUE` forms (#1301).
const SENSITIVE_ARG_FLAGS: &[&str] = &["--token", "--api-key", "--password", "--secret", "--key"];

/// Redact credentials embedded in a URL for an audit dump: the userinfo
/// (`user:pass@`) and any query-param value whose key is sensitive
/// ([`SENSITIVE_QUERY_KEYS`]). Non-secret structure (scheme, host, path,
/// fragment, non-sensitive params) is preserved. Pure string surgery — no `url`
/// crate dependency.
fn redact_url_secrets(url: &str) -> String {
    // Peel off `#fragment` then `?query`, redact each part, reassemble.
    let (main, fragment) = match url.split_once('#') {
        Some((m, f)) => (m, Some(f)),
        None => (url, None),
    };
    let (authority_and_path, query) = match main.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (main, None),
    };
    let mut out = redact_url_userinfo(authority_and_path);
    if let Some(q) = query {
        out.push('?');
        out.push_str(&redact_url_query(q));
    }
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

/// Redact `user:pass@` userinfo from the authority of a `scheme://…` string
/// (the `?query`/`#fragment` already stripped). An `@` only counts inside the
/// authority (before the first `/`), so a path/param `@` is never mistaken for
/// userinfo.
fn redact_url_userinfo(s: &str) -> String {
    let Some((scheme, rest)) = s.split_once("://") else {
        return s.to_string();
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let authority = match authority.rsplit_once('@') {
        Some((_userinfo, host)) => format!("{}@{host}", Config::REDACTED),
        None => authority.to_string(),
    };
    let mut out = format!("{scheme}://{authority}");
    if let Some(p) = path {
        out.push('/');
        out.push_str(p);
    }
    out
}

/// Redact the values of sensitive query params, keeping keys + non-sensitive
/// params intact.
fn redact_url_query(query: &str) -> String {
    query
        .split('&')
        .map(|param| match param.split_once('=') {
            Some((k, _))
                if SENSITIVE_QUERY_KEYS
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(k)) =>
            {
                format!("{k}={}", Config::REDACTED)
            }
            _ => param.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Whether `flag` is a sensitive CLI flag whose value must be redacted.
fn is_sensitive_arg_flag(flag: &str) -> bool {
    SENSITIVE_ARG_FLAGS
        .iter()
        .any(|s| s.eq_ignore_ascii_case(flag))
}

/// Redact the values of sensitive flags in an args vector, handling both
/// `--flag=VALUE` (redact the tail) and `--flag VALUE` (redact the next arg).
/// Over-redaction is safe for an audit dump; under-redaction is not.
fn redact_arg_secrets(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut redact_next = false;
    for arg in args {
        if redact_next {
            out.push(Config::REDACTED.to_string());
            redact_next = false;
            continue;
        }
        match arg.split_once('=') {
            Some((flag, _)) if is_sensitive_arg_flag(flag) => {
                out.push(format!("{flag}={}", Config::REDACTED));
            }
            _ if is_sensitive_arg_flag(arg) => {
                // `--flag VALUE`: keep the flag, redact the following value.
                out.push(arg.clone());
                redact_next = true;
            }
            _ => out.push(arg.clone()),
        }
    }
    out
}

/// Best-effort home directory lookup without pulling in the `dirs` crate.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .filter(|s| !s.is_empty())
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

/// Stamp the MCP servers that originated from the walked-up project-local
/// `.newt/config.toml` as [`crate::mcp::McpTrust::Untrusted`] — the #1301 trust
/// boundary for a cloned repo's ambient config.
///
/// By the time [`Config::resolve`] has a typed `Config`, the project entries are
/// already folded into `servers` by [`merge_toml`] and — because `trust` is
/// `#[serde(skip)]` — every entry carries the `Trusted` default, so provenance
/// is reconstructed from the merge shape (which must match [`merge_toml`]):
/// - `project_mcp_count == None` → the project file had no `mcp_servers` key, so
///   `servers` came wholly from the trusted base (user config) → mark none.
/// - [`ArrayMergeStrategy::Replace`] with a project `mcp_servers` array present →
///   the project array REPLACED the base's, so every entry is project-origin.
/// - [`ArrayMergeStrategy::Append`] → the project entries were concatenated
///   AFTER the base's (base first), so only the trailing `count` are
///   project-origin.
///
/// Only ever downgrades (Trusted → Untrusted); it never elevates, so a genuine
/// user-config entry can never be mislabeled trusted by this path.
fn mark_project_mcp_untrusted(
    servers: &mut [crate::mcp::McpServerEntry],
    strategy: ArrayMergeStrategy,
    project_mcp_count: Option<usize>,
) {
    let Some(count) = project_mcp_count else {
        return;
    };
    let start = match strategy {
        // Replace swapped the whole array for the project's — all project-origin.
        ArrayMergeStrategy::Replace => 0,
        // Append put the project entries last — mark only that trailing slice.
        ArrayMergeStrategy::Append => servers.len().saturating_sub(count),
    };
    for entry in &mut servers[start..] {
        entry.trust = crate::mcp::McpTrust::Untrusted;
    }
}

/// Whether the resolved base config is the AMBIENT cwd-relative `./newt.toml`
/// candidate (a freshly cloned repo can ship one at its root — the #1301 sibling
/// of the walked-up `.newt/config.toml` vector), as opposed to an
/// operator-explicit base.
///
/// The only base a caller can pin explicitly *through [`Config::resolve`]* is
/// `$NEWT_CONFIG` (the `--config` flag routes through [`Config::load`], which
/// never reaches `resolve`, so it is Trusted without touching this path). So the
/// `./newt.toml` base is explicit — Trusted — ONLY when `$NEWT_CONFIG` points AT
/// it; the implicit fallthrough to the `./newt.toml` candidate (`$NEWT_CONFIG`
/// unset, or set to some other/broken path) is ambient — Untrusted.
fn base_is_ambient_newt_toml(base: Option<&Path>) -> bool {
    let ambient_candidate = Path::new("./newt.toml");
    if base != Some(ambient_candidate) {
        return false;
    }
    // Mirror `candidate_paths`' `env::var("NEWT_CONFIG")` read: only a
    // `$NEWT_CONFIG` that *is* `./newt.toml` selected this base explicitly.
    match std::env::var("NEWT_CONFIG") {
        Ok(explicit) => Path::new(&explicit) != ambient_candidate,
        Err(_) => true,
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

/// Walk `start` and its ancestors, returning the first `ancestor.join(rel)` for
/// which `exists` is true. Pure: the filesystem probe is the injected `exists`
/// closure, so the walk logic is unit-testable without touching disk.
pub(crate) fn find_ancestor_dir(
    start: &Path,
    rel: &Path,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        let candidate = ancestor.join(rel);
        if exists(&candidate) {
            return Some(candidate);
        }
    }
    None
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

    // ── color / theme mode (issue #527) ─────────────────────────────────

    #[test]
    fn color_mode_defaults_to_auto_and_round_trips() {
        // Absent key → Auto (color on a TTY, none off one).
        let cfg: TuiConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.color, ColorMode::Auto);
        // Every keyword parses from its serde (lowercase) key.
        for (key, want) in [
            ("auto", ColorMode::Auto),
            ("always", ColorMode::Always),
            ("never", ColorMode::Never),
            ("minimal", ColorMode::Minimal),
            ("inverted", ColorMode::Inverted),
            ("dark", ColorMode::Dark),
            ("light", ColorMode::Light),
            ("mono", ColorMode::Mono),
        ] {
            let cfg: TuiConfig = toml::from_str(&format!("color = \"{key}\"")).unwrap();
            assert_eq!(cfg.color, want, "color = {key}");
        }
    }

    #[test]
    fn color_mode_keyword_round_trips_and_aliases_parse() {
        // keyword() is the inverse of from_keyword() for every canonical variant.
        for m in [
            ColorMode::Auto,
            ColorMode::Always,
            ColorMode::Never,
            ColorMode::Minimal,
            ColorMode::Inverted,
            ColorMode::Dark,
            ColorMode::Light,
            ColorMode::Mono,
        ] {
            assert_eq!(ColorMode::from_keyword(m.keyword()), Some(m));
        }
        // Case-insensitive + aliases.
        assert_eq!(ColorMode::from_keyword("ALWAYS"), Some(ColorMode::Always));
        assert_eq!(ColorMode::from_keyword(" on "), Some(ColorMode::Always));
        assert_eq!(ColorMode::from_keyword("off"), Some(ColorMode::Never));
        assert_eq!(ColorMode::from_keyword("monochrome"), Some(ColorMode::Mono));
        // Unknown keyword is rejected (the CLI value_parser surfaces this).
        assert_eq!(ColorMode::from_keyword("rainbow"), None);
    }

    #[test]
    fn color_mode_forced_and_is_mono() {
        // forced(): Some(true) = color on, Some(false) = off, None = defer to TTY.
        assert_eq!(ColorMode::Always.forced(), Some(true));
        assert_eq!(ColorMode::Dark.forced(), Some(true));
        assert_eq!(ColorMode::Light.forced(), Some(true));
        assert_eq!(ColorMode::Inverted.forced(), Some(true));
        assert_eq!(ColorMode::Minimal.forced(), Some(true));
        assert_eq!(ColorMode::Never.forced(), Some(false));
        assert_eq!(ColorMode::Mono.forced(), Some(false));
        assert_eq!(ColorMode::Auto.forced(), None);
        // is_mono distinguishes the ASCII-fallback mode from plain Never.
        assert!(ColorMode::Mono.is_mono());
        assert!(!ColorMode::Never.is_mono());
        assert!(!ColorMode::Auto.is_mono());
    }

    #[test]
    fn markdown_mode_defaults_to_auto_round_trips_and_forces() {
        assert_eq!(MarkdownMode::default(), MarkdownMode::Auto);
        for m in [MarkdownMode::Auto, MarkdownMode::On, MarkdownMode::Off] {
            assert_eq!(MarkdownMode::from_keyword(m.keyword()), Some(m));
        }
        // Case-insensitive + always/never aliases.
        assert_eq!(MarkdownMode::from_keyword("ON"), Some(MarkdownMode::On));
        assert_eq!(
            MarkdownMode::from_keyword(" always "),
            Some(MarkdownMode::On)
        );
        assert_eq!(MarkdownMode::from_keyword("never"), Some(MarkdownMode::Off));
        assert_eq!(MarkdownMode::from_keyword("rainbow"), None);
        // forced(): On = Some(true), Off = Some(false), Auto = defer.
        assert_eq!(MarkdownMode::On.forced(), Some(true));
        assert_eq!(MarkdownMode::Off.forced(), Some(false));
        assert_eq!(MarkdownMode::Auto.forced(), None);
    }

    #[test]
    fn tui_markdown_parses_from_toml_and_defaults_to_auto() {
        let cfg: TuiConfig = toml::from_str("markdown = \"off\"").unwrap();
        assert_eq!(cfg.markdown, MarkdownMode::Off);
        let default: TuiConfig = toml::from_str("").unwrap();
        assert_eq!(default.markdown, MarkdownMode::Auto);
    }

    /// Step 24.10 (#559): summarizer knobs live in `summarizer.toml` now.
    /// Defaults (absent file) reuse the session backend; timeout 60 / retries 1.
    #[test]
    fn backend_kind_embedded_parses_and_labels() {
        // #639: the config accepts `kind = "embedded"` so the summarizer (and a
        // backend) can select the in-process backend.
        #[derive(serde::Deserialize)]
        struct K {
            kind: BackendKind,
        }
        let k: K = toml::from_str("kind = \"embedded\"").unwrap();
        assert_eq!(k.kind, BackendKind::Embedded);
        assert_eq!(k.kind.label(), "embedded");
    }

    #[test]
    fn summarizer_config_defaults_and_parse() {
        let d = SummarizerConfig::default();
        assert_eq!(d.endpoint, None);
        assert_eq!(d.model, None);
        assert_eq!(d.kind, None);
        assert_eq!(d.timeout_secs, 60);
        assert_eq!(d.retries, 1);
        assert_eq!(d.fallback_model, None);

        let cfg = SummarizerConfig::from_toml_str(
            "endpoint = \"http://REDACTED-HOST:11434\"\n\
             model = \"qwen2.5-coder:3b\"\n\
             kind = \"openai\"\n\
             timeout_secs = 45\n\
             retries = 2\n\
             fallback_model = \"nemotron-mini:4b\"\n\
             keep_alive = \"10m\"",
        )
        .unwrap();
        assert_eq!(cfg.endpoint.as_deref(), Some("http://REDACTED-HOST:11434"));
        assert_eq!(cfg.model.as_deref(), Some("qwen2.5-coder:3b"));
        assert_eq!(cfg.kind, Some(BackendKind::Openai));
        assert_eq!(cfg.timeout_secs, 45);
        assert_eq!(cfg.retries, 2);
        assert_eq!(cfg.fallback_model.as_deref(), Some("nemotron-mini:4b"));
        assert_eq!(cfg.keep_alive.as_deref(), Some("10m"));
    }

    /// A partial file fills only the keys present; the rest stay at defaults
    /// (so an `endpoint`-only file reuses the session model but a fast box).
    #[test]
    fn summarizer_config_partial_keeps_defaults() {
        let cfg = SummarizerConfig::from_toml_str("endpoint = \"http://fast.box:11434\"").unwrap();
        assert_eq!(cfg.endpoint.as_deref(), Some("http://fast.box:11434"));
        assert_eq!(cfg.model, None); // reuse session model
        assert_eq!(cfg.timeout_secs, 60); // default
        assert_eq!(cfg.retries, 1); // default
    }

    #[test]
    fn context_manager_keyword_roundtrip_and_availability() {
        for m in [
            ContextManager::Standard,
            ContextManager::Progressive,
            ContextManager::Distributed,
        ] {
            assert_eq!(ContextManager::from_keyword(m.keyword()), Some(m));
        }
        assert_eq!(
            ContextManager::from_keyword("  STANDARD "),
            Some(ContextManager::Standard),
            "case/space-insensitive"
        );
        assert_eq!(ContextManager::from_keyword("nope"), None);
        // Only standard is implemented today; the others are pending #546.
        assert!(ContextManager::Standard.available());
        assert!(!ContextManager::Progressive.available());
        assert!(!ContextManager::Distributed.available());
        assert_eq!(ContextManager::default(), ContextManager::Standard);
    }

    #[test]
    fn compaction_trigger_policy_keyword_roundtrip_and_default() {
        for policy in [
            CompactionTriggerPolicy::HeadroomAware,
            CompactionTriggerPolicy::MessageCount,
        ] {
            assert_eq!(
                CompactionTriggerPolicy::from_keyword(policy.as_str()),
                Some(policy)
            );
            assert_eq!(policy.keyword(), policy.as_str());
        }
        assert_eq!(
            CompactionTriggerPolicy::from_keyword("  MESSAGE_COUNT "),
            Some(CompactionTriggerPolicy::MessageCount),
            "case/space-insensitive"
        );
        assert_eq!(CompactionTriggerPolicy::from_keyword("nope"), None);
        assert_eq!(
            CompactionTriggerPolicy::default(),
            CompactionTriggerPolicy::HeadroomAware
        );
    }

    #[test]
    fn context_section_defaults_and_parses() {
        // Absent [context] → None on Config; the resolver falls back to standard.
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.context.is_none());
        let c: ContextConfig = toml::from_str(
            "manager = \"progressive\"\ncompaction_trigger_policy = \"message_count\"",
        )
        .unwrap();
        assert_eq!(c.manager, ContextManager::Progressive);
        assert_eq!(
            c.compaction_trigger_policy,
            CompactionTriggerPolicy::MessageCount
        );
        let defaults = ContextConfig::default();
        assert_eq!(defaults.manager, ContextManager::Standard);
        assert_eq!(
            defaults.compaction_trigger_policy,
            CompactionTriggerPolicy::HeadroomAware
        );
        // Omitting the key uses the serde default rather than requiring every
        // existing `[context]` configuration to opt in explicitly.
        let parsed_default: ContextConfig = toml::from_str("manager = \"standard\"").unwrap();
        assert_eq!(
            parsed_default.compaction_trigger_policy,
            CompactionTriggerPolicy::HeadroomAware
        );
        assert!(
            toml::from_str::<ContextConfig>("compaction_trigger_policy = \"not_a_policy\"")
                .is_err(),
            "an invalid policy must fail config parsing rather than silently changing safety behavior"
        );
    }

    #[test]
    fn context_input_ceiling_pct_normalizes_at_deserialization_boundary() {
        for value in [1, 80, 99] {
            let parsed: ContextConfig =
                toml::from_str(&format!("input_ceiling_pct = {value}")).unwrap();
            assert_eq!(parsed.input_ceiling_pct, value);
        }

        for value in [0, 100, 101, u32::MAX] {
            let parsed: ContextConfig =
                toml::from_str(&format!("input_ceiling_pct = {value}")).unwrap();
            assert_eq!(
                parsed.input_ceiling_pct,
                default_input_ceiling_pct(),
                "out-of-range value {value} must fall back to the documented safe default"
            );
        }

        assert_eq!(input_percentage_ceiling(32_768, 90), 29_491);
        assert_eq!(
            input_percentage_ceiling(32_768, 0),
            26_214,
            "programmatic callers share the same invalid-value fallback",
        );
    }

    #[test]
    fn scratch_section_defaults_and_parses() {
        // #844: `[scratch] dir` parses onto Config; absent → None (the `.scratch`
        // default applies at resolution). Uses `from_str` (not `resolve`) so this
        // does NOT publish a process-global scratch dir.
        let bare: Config = toml::from_str("").unwrap();
        assert!(bare.scratch.is_none());
        let cfg: Config = toml::from_str("[scratch]\ndir = \"/tmp/newt-scratch\"\n").unwrap();
        assert_eq!(
            cfg.scratch.and_then(|s| s.dir).as_deref(),
            Some("/tmp/newt-scratch")
        );
    }

    #[test]
    fn semantic_config_defaults_and_parses() {
        // Defaults (Step 26.5.4): nomic-embed-text, top_k 5, no decoupled
        // endpoint, and on_embed_failure = disable (the safe default).
        let d = SemanticConfig::default();
        assert_eq!(d.embedding_model, "nomic-embed-text");
        assert_eq!(d.top_k, 5);
        assert_eq!(d.embeddings_endpoint, None);
        assert_eq!(d.embeddings_api, None);
        assert_eq!(d.on_embed_failure, OnEmbedFailure::Disable);
        // #720: the embedded-embedder local model dir defaults to None.
        assert_eq!(d.embedding_model_path, None);
        // `[context.semantic]` parses + overrides, incl. the new fields.
        let c: ContextConfig = toml::from_str(
            "[semantic]\nembedding_model = \"mxbai-embed-large\"\ntop_k = 8\n\
             embedding_model_path = \"/models/bge-small-en-v1.5\"\n\
             embeddings_endpoint = \"http://REDACTED-HOST:11434\"\n\
             embeddings_api = \"ollama\"\non_embed_failure = \"warn\"",
        )
        .unwrap();
        assert_eq!(c.semantic.embedding_model, "mxbai-embed-large");
        assert_eq!(
            c.semantic.embedding_model_path.as_deref(),
            Some("/models/bge-small-en-v1.5")
        );
        assert_eq!(c.semantic.top_k, 8);
        assert_eq!(
            c.semantic.embeddings_endpoint.as_deref(),
            Some("http://REDACTED-HOST:11434")
        );
        assert_eq!(c.semantic.embeddings_api, Some(BackendKind::Ollama));
        assert_eq!(c.semantic.on_embed_failure, OnEmbedFailure::Warn);
        // `embeddings_api = "vllm"` aliases to the OpenAI protocol.
        let v: ContextConfig = toml::from_str("[semantic]\nembeddings_api = \"vllm\"").unwrap();
        assert_eq!(v.semantic.embeddings_api, Some(BackendKind::Openai));
        // an absent [context.semantic] still yields the defaults
        let bare: ContextConfig = toml::from_str("manager = \"standard\"").unwrap();
        assert_eq!(bare.semantic, SemanticConfig::default());
    }

    #[test]
    fn context_feature_keyword_alias_availability_and_issue() {
        // canonical keyword round-trips
        for f in ContextFeature::ALL {
            assert_eq!(ContextFeature::from_keyword(f.keyword()), Some(f));
        }
        // aliases + hyphen/underscore/case
        assert_eq!(
            ContextFeature::from_keyword("TOOL-OFFLOAD"),
            Some(ContextFeature::ToolOffload)
        );
        assert_eq!(
            ContextFeature::from_keyword("offload"),
            Some(ContextFeature::ToolOffload)
        );
        assert_eq!(
            ContextFeature::from_keyword(" state "),
            Some(ContextFeature::Scratchpad)
        );
        assert_eq!(ContextFeature::from_keyword("nope"), None);
        // tool_offload (26.3), scratchpad (26.4), semantic (26.5), experiential
        // (26.6a), scheduled (26.6b) shipped; only provenance is still pending.
        assert!(ContextFeature::ToolOffload.available());
        assert!(ContextFeature::Scratchpad.available());
        assert!(ContextFeature::Semantic.available());
        assert!(ContextFeature::Experiential.available());
        assert!(ContextFeature::Scheduled.available());
        assert!(
            !ContextFeature::Provenance.available(),
            "provenance still pending"
        );
        assert!(ContextFeature::ALL
            .iter()
            .filter(|f| !matches!(f, ContextFeature::Provenance))
            .all(|f| f.available()));
        // issues route to the right tracking ticket
        assert_eq!(ContextFeature::Semantic.issue(), 582);
        assert_eq!(ContextFeature::Scratchpad.issue(), 583);
        assert_eq!(ContextFeature::ToolOffload.issue(), 584);
        assert_eq!(ContextFeature::Provenance.issue(), 584);
        assert_eq!(ContextFeature::Experiential.issue(), 585);
        assert_eq!(ContextFeature::Scheduled.issue(), 586);
    }

    #[test]
    fn context_features_override_layering_and_parse() {
        use ContextFeature as F;
        // Every preset resolves to all-off today (standard behavior).
        let base = ContextManager::Standard.base_features();
        assert!(base.enabled().is_empty());
        // An override layers on top of the base, leaving others untouched.
        let mut ov = ContextFeatures::default();
        ov.set(F::Scratchpad, Some(true));
        let resolved = ov.apply_to(base);
        assert!(resolved.get(F::Scratchpad));
        assert!(!resolved.get(F::Semantic));
        assert_eq!(resolved.enabled(), vec![F::Scratchpad]);
        // None override = inherit (no change); Some(false) = force off.
        let mut ov2 = ContextFeatures::default();
        ov2.set(F::Scratchpad, Some(false));
        assert!(!ov2.apply_to(resolved).get(F::Scratchpad));
        // [context.features] parses keyed by canonical keyword.
        let c: ContextConfig = toml::from_str(
            "manager = \"standard\"\n[features]\nsemantic = true\nscratchpad = false",
        )
        .unwrap();
        assert_eq!(c.features.get(F::Semantic), Some(true));
        assert_eq!(c.features.get(F::Scratchpad), Some(false));
        assert_eq!(c.features.get(F::ToolOffload), None);
    }

    #[test]
    fn base_for_defaults_tool_offload_on_and_local_assist_on_for_ollama() {
        use ContextFeature as F;
        // #945: tool offload is local spill storage and defaults ON for every
        // backend. Step 27.4: local (Ollama) backends additionally default
        // scratchpad + scheduled ON; semantic also defaults ON but degrades to a
        // one-shot no-op until an embedder is configured.
        let local = ContextFeatureSet::base_for(ContextManager::Standard, BackendKind::Ollama);
        assert!(local.get(F::ToolOffload));
        assert!(local.get(F::Scratchpad));
        assert!(local.get(F::Semantic));
        assert!(local.get(F::Scheduled));
        // Cloud (OpenAI-compatible): per the user's context policy, every
        // available feature defaults ON except Provenance, regardless of
        // backend. Semantic degrades to a no-op until an embedder is set.
        let cloud = ContextFeatureSet::base_for(ContextManager::Standard, BackendKind::Openai);
        assert!(cloud.get(F::ToolOffload));
        assert!(cloud.get(F::Scratchpad));
        assert!(cloud.get(F::Semantic));
        assert!(cloud.get(F::Scheduled));
        // An explicit override still wins over the local default (force off).
        let mut ov = ContextFeatures::default();
        ov.set(F::Scheduled, Some(false));
        ov.set(F::ToolOffload, Some(false));
        assert!(!ov.apply_to(local).get(F::Scheduled));
        assert!(!ov.apply_to(local).get(F::ToolOffload));
        assert!(ov.apply_to(local).get(F::Scratchpad)); // untouched feature stays on
    }

    #[test]
    fn allow_bang_escape_defaults_to_true_and_round_trips() {
        // Absent key → enabled (the human's host shell-out is on by default).
        let cfg: TuiConfig = toml::from_str("").unwrap();
        assert!(cfg.allow_bang_escape);
        // Explicit opt-out parses.
        let cfg: TuiConfig = toml::from_str("allow_bang_escape = false").unwrap();
        assert!(!cfg.allow_bang_escape);
    }

    #[test]
    fn shell_commands_default_on_mutations_default_off_and_round_trip() {
        // Navigation/inspection suite on by default; mutations off until opted in.
        let cfg: TuiConfig = toml::from_str("").unwrap();
        assert!(cfg.allow_shell_commands);
        assert!(!cfg.allow_shell_mutations);
        let cfg: TuiConfig =
            toml::from_str("allow_shell_commands = false\nallow_shell_mutations = true").unwrap();
        assert!(!cfg.allow_shell_commands);
        assert!(cfg.allow_shell_mutations);
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
            [[backends]]
            name = "real-box"
            endpoint = "http://h:11434"
            model = "m"

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
        // dangling provider — must name a [backends] entry (Slice 2). The error
        // lists the known backends, here the explicit `real-box`.
        let bad_provider = Loadout {
            provider: Some("ghost-provider".into()),
            ..Default::default()
        };
        let e = bad_provider.validate(&cfg).unwrap_err();
        assert!(
            e.contains("provider 'ghost-provider'")
                && e.contains("no [backends] entry")
                && e.contains("real-box"),
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
    fn crew_parses_inline_and_validates_role_references() {
        let cfg: Config = toml::from_str(
            r#"
            [[backends]]
            name = "dgx"
            endpoint = "http://dgx.local:11434"
            model = "qwen3-coder:30b"
            tiers = []
            [[backends]]
            name = "gnuc"
            endpoint = "http://localhost:11434"
            model = "qwen2.5-coder:3b"
            tiers = []

            [loadouts.planner]
            provider = "dgx"
            [loadouts.navigator]
            provider = "dgx"
            [loadouts.triage]
            provider = "gnuc"

            [crews.coder]
            planner = "planner"
            navigator = "navigator"
            triage = "triage"
            loop = "patch-revise"
            [crews.coder.budgets]
            max_attempts = 4
            require_human_review_on = ["auth", "crypto"]
            "#,
        )
        .unwrap();
        let c = &cfg.crews["coder"];
        assert_eq!(c.planner, "planner");
        assert_eq!(c.navigator.as_deref(), Some("navigator"));
        assert_eq!(c.loop_program.as_deref(), Some("patch-revise"));
        assert_eq!(c.budgets.as_ref().unwrap().max_attempts, Some(4));
        // each role names a known loadout, and each loadout validates
        assert!(c.validate(&cfg).is_ok());
    }

    #[test]
    fn crew_rejects_dangling_and_invalid_roles() {
        let cfg: Config = toml::from_str(
            r#"
            [[backends]]
            name = "dgx"
            endpoint = "http://dgx.local:11434"
            model = "m"
            tiers = []
            [loadouts.planner]
            provider = "dgx"
            "#,
        )
        .unwrap();
        // dangling role: triage names no loadout
        let dangling = Crew {
            planner: "planner".into(),
            triage: Some("ghost".into()),
            ..Default::default()
        };
        let e = dangling.validate(&cfg).unwrap_err();
        assert!(e.contains("triage 'ghost'"), "{e}");
        assert!(e.contains("no [loadouts]"), "{e}");
        // transitive: a role's loadout has a dangling provider
        let mut cfg2 = cfg.clone();
        cfg2.loadouts.insert(
            "bad".into(),
            Loadout {
                provider: Some("nope".into()),
                ..Default::default()
            },
        );
        let transitive = Crew {
            planner: "bad".into(),
            ..Default::default()
        };
        let e = transitive.validate(&cfg2).unwrap_err();
        assert!(
            e.contains("planner 'bad'") && e.contains("provider 'nope'"),
            "{e}"
        );
    }

    #[test]
    fn disk_crews_load_per_file_by_stem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("coder.toml"),
            "planner = \"planner\"\nnavigator = \"navigator\"\n",
        )
        .unwrap();
        // malformed (missing required `planner`) is skipped, not fatal
        std::fs::write(dir.path().join("broken.toml"), "navigator = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "not a crew").unwrap();

        let mut cfg = Config::default();
        cfg.merge_crews_from_dir(dir.path());
        assert_eq!(cfg.crews.len(), 1, "only the valid .toml loads");
        let c = cfg.crews.get("coder").expect("loaded by filename stem");
        assert_eq!(c.planner, "planner");
        // disk overrides inline of the same name (last-wins)
        cfg.crews.insert(
            "coder".into(),
            Crew {
                planner: "inline".into(),
                ..Default::default()
            },
        );
        cfg.merge_crews_from_dir(dir.path());
        assert_eq!(cfg.crews["coder"].planner, "planner", "disk wins");
    }

    #[test]
    fn backend_api_axis_defaults_and_parses() {
        // Absent → unset (probe-at-connect for openai backends).
        let def: BackendConfig =
            toml::from_str("endpoint=\"http://h:1\"\nmodel=\"m\"\nkind=\"openai\"\n").unwrap();
        assert_eq!(def.api, None);
        // Explicit responses opt-in.
        let resp: BackendConfig = toml::from_str(
            "endpoint=\"http://h:1\"\nmodel=\"gpt-5-codex\"\nkind=\"openai\"\napi=\"responses\"\n",
        )
        .unwrap();
        assert_eq!(resp.api, Some(OpenAiApi::Responses));
        // `chat` is an accepted alias for chat_completions.
        let alias: BackendConfig =
            toml::from_str("endpoint=\"http://h:1\"\nmodel=\"m\"\napi=\"chat\"\n").unwrap();
        assert_eq!(alias.api, Some(OpenAiApi::ChatCompletions));
    }

    #[test]
    fn discovery_defaults_cover_localhost_unboxing() {
        // #1130: absent [discovery] seeds the localhost sweep — ollama's port
        // plus the vLLM range (several ports = several one-model instances).
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.discovery.hosts, vec!["localhost".to_string()]);
        assert_eq!(cfg.discovery.ollama_ports, vec![11434]);
        assert_eq!(cfg.discovery.vllm_ports, vec![8000, 8080, 8001, 8002, 8003]);
        assert_eq!(cfg.default_backend, None);

        // Declared values override wholesale (no merge magic).
        let cfg: Config = toml::from_str(
            "default_backend=\"dgx1-vllm\"\n[discovery]\nhosts=[\"localhost\",\"dgx1\"]\nvllm_ports=[8000]\n",
        )
        .unwrap();
        assert_eq!(cfg.default_backend.as_deref(), Some("dgx1-vllm"));
        assert_eq!(cfg.discovery.hosts.len(), 2);
        assert_eq!(cfg.discovery.vllm_ports, vec![8000]);
        // Unlisted keys keep their defaults ([serde(default)] per-field).
        assert_eq!(cfg.discovery.ollama_ports, vec![11434]);
    }

    #[test]
    fn serving_axis_fields_round_trip_and_stay_minimal() {
        // #1129 (epic #1126): the serving axis + host/coexist/ram_gib/card/
        // capability/provenance are all OPTIONAL — a legacy file with none of
        // them parses (None everywhere), and a full file round-trips.
        let legacy: BackendConfig =
            toml::from_str("endpoint=\"http://h:1\"\nmodel=\"m\"\n").unwrap();
        assert_eq!(legacy.serving, None);
        assert_eq!(legacy.host, None);
        assert_eq!(legacy.coexist, None);
        assert_eq!(legacy.managed, None);

        let full: BackendConfig = toml::from_str(
            "endpoint=\"http://dgx:8000\"\nkind=\"openai\"\nserving=\"multiplexer\"\n\
             managed=\"shared\"\n\
             host=\"dgx1\"\ncoexist=true\nram_gib=480.0\ncard=\"ornith-1.0-35b\"\n\
             [capability]\nthinking_default=true\n\
             [provenance]\nsource=\"newt setup v0.7.3\"\nderived_serving=true\n",
        )
        .unwrap();
        assert_eq!(full.serving, Some(Serving::Multiplexer));
        assert_eq!(full.managed, Some(ManagedMode::Shared));
        assert_eq!(full.host.as_deref(), Some("dgx1"));
        assert_eq!(full.coexist, Some(true));
        assert_eq!(full.ram_gib, Some(480.0));
        assert_eq!(full.card.as_deref(), Some("ornith-1.0-35b"));
        assert_eq!(
            full.capability.as_ref().and_then(|c| c.thinking_default),
            Some(true)
        );
        assert_eq!(
            full.provenance.as_ref().and_then(|p| p.derived_serving),
            Some(true)
        );

        // Serialization stays minimal: unset optional fields are skipped, so a
        // generated backends/<name>.toml doesn't bloat with nulls.
        let out = toml::to_string(&legacy).unwrap();
        assert!(!out.contains("serving"), "unset fields are skipped: {out}");
        assert!(!out.contains("managed"), "unset managed is skipped: {out}");
        assert!(!out.contains("provenance"));
    }

    #[test]
    fn backend_reasoning_replay_scope_is_explicit_and_defaults_never() {
        let default_backend: BackendConfig =
            toml::from_str("endpoint=\"http://h:1\"\nmodel=\"m\"\n").unwrap();
        assert_eq!(
            default_backend.reasoning_replay_scope(),
            crate::model_card::ReasoningReplayScope::Never
        );

        let replay_backend: BackendConfig = toml::from_str(
            "endpoint=\"http://h:1\"\nmodel=\"m\"\n\
             [capability]\nreasoning_replay_scope=\"current_user_turn\"\n",
        )
        .unwrap();
        assert_eq!(
            replay_backend.reasoning_replay_scope(),
            crate::model_card::ReasoningReplayScope::CurrentUserTurn
        );
    }

    #[test]
    fn backend_chat_completions_generation_policy_is_explicit_capability_data() {
        let backend: BackendConfig = toml::from_str(
            "endpoint=\"http://h:1\"\nmodel=\"m\"\nkind=\"openai\"\n\
             [capability.chat_completions]\ncognition=true\n\
             chat_template_kwargs=true\nparallel_tool_calls=false\n\
             bounded_reasoning_continuation=true\n",
        )
        .expect("chat-completions policy is valid capability data");

        let capability = serde_json::to_value(backend.capability.expect("capability present"))
            .expect("capability serializes");
        assert_eq!(capability["chat_completions"]["cognition"], true);
        assert_eq!(capability["chat_completions"]["chat_template_kwargs"], true);
        assert_eq!(capability["chat_completions"]["parallel_tool_calls"], false);
        assert_eq!(
            capability["chat_completions"]["bounded_reasoning_continuation"],
            true
        );
    }

    #[test]
    fn derive_serving_rules() {
        // Ollama is ALWAYS a multiplexer, even with one model pulled today.
        assert_eq!(derive_serving(BackendKind::Ollama, 1), Serving::Multiplexer);
        assert_eq!(derive_serving(BackendKind::Ollama, 7), Serving::Multiplexer);
        // A vLLM instance declares exactly one model on /v1/models.
        assert_eq!(derive_serving(BackendKind::Openai, 1), Serving::Instance);
        // An OpenAI-compatible gateway fronting a fleet lists many.
        assert_eq!(derive_serving(BackendKind::Openai, 3), Serving::Multiplexer);
        // The in-process engine runs one GGUF.
        assert_eq!(derive_serving(BackendKind::Embedded, 1), Serving::Instance);
    }

    #[test]
    fn mcp_stdio_env_allowlist_excludes_secrets_and_is_closed() {
        // #1155: the stdio-MCP env allow-list must NOT be a passthrough of the
        // whole environment — secret-bearing vars are absent, and it stays a
        // superset of the shell default (a subprocess needs PATH to exec).
        let allow = mcp_stdio_env_passthrough();
        for secret in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "DGX_API_KEY",
            "NVIDIA_API_KEY",
            // The encrypted-token-store unlock channel (crate::secrets):
            // a child process must never inherit the vault passphrase.
            crate::secrets::PASSPHRASE_ENV,
        ] {
            assert!(!allow.contains(&secret), "{secret} must never be inherited");
        }
        assert!(allow.contains(&"PATH"), "a child needs PATH to exec");
        for base in shell_env_passthrough_default() {
            assert!(
                allow.contains(&base.as_str()),
                "{base} (shell default) should be covered"
            );
        }
    }

    #[test]
    fn backend_model_is_optional_and_read_via_effective_model() {
        // #1128 (epic #1126): a model-less backend file PARSES — "the server
        // dictates"; Phase B's adopt() fills it at session start. Previously
        // `model` was required, so such a drop-in failed to parse and was
        // silently skipped.
        let serverless: BackendConfig =
            toml::from_str("endpoint=\"http://h:8000\"\nkind=\"openai\"\n").unwrap();
        assert_eq!(serverless.model, None);
        assert_eq!(serverless.effective_model(), None);

        // A declared model reads through effective_model unchanged.
        let pinned: BackendConfig =
            toml::from_str("endpoint=\"http://h:1\"\nmodel=\"qwen3:32b\"\n").unwrap();
        assert_eq!(pinned.effective_model(), Some("qwen3:32b"));

        // An EMPTY model string counts as unset — it must never be sent as a
        // model name in a request.
        let empty: BackendConfig = toml::from_str("endpoint=\"http://h:1\"\nmodel=\"\"\n").unwrap();
        assert_eq!(empty.effective_model(), None);
    }

    #[test]
    fn disk_backends_load_per_file_by_stem_and_override_inline() {
        let dir = tempfile::tempdir().unwrap();
        // A minimal drop-in: name omitted (filename is authoritative), tiers
        // omitted (defaults empty), kind omitted (defaults ollama).
        std::fs::write(
            dir.path().join("dgx1.toml"),
            "endpoint = \"http://REDACTED-HOST:11434\"\nmodel = \"qwen3:30b\"\n",
        )
        .unwrap();
        // Malformed (missing required `endpoint`) is skipped, not fatal.
        std::fs::write(dir.path().join("broken.toml"), "model = \"x\"\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "not a backend").unwrap();

        let mut cfg = Config {
            // An inline backend of the same name that the drop-in should replace,
            // plus an unrelated one that must survive untouched.
            backends: vec![
                BackendConfig {
                    name: "dgx1".into(),
                    endpoint: "http://stale:11434".into(),
                    model: Some("old-model".into()),
                    model_path: None,
                    tiers: vec![],
                    kind: Some(BackendKind::Ollama),
                    api: Default::default(),
                    api_key_file: None,
                    api_key_env: None,
                    ..Default::default()
                },
                BackendConfig {
                    name: "gnuc".into(),
                    endpoint: "http://gnuc:11434".into(),
                    model: Some("qwen2.5-coder:14b".into()),
                    model_path: None,
                    tiers: vec![],
                    kind: Some(BackendKind::Ollama),
                    api: Default::default(),
                    api_key_file: None,
                    api_key_env: None,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        cfg.merge_backends_from_dir(dir.path());

        // The drop-in replaced the inline dgx1 in place (no duplicate), gnuc kept.
        assert_eq!(cfg.backends.len(), 2, "only the valid .toml loads, no dup");
        let dgx1 = cfg.backends.iter().find(|b| b.name == "dgx1").unwrap();
        assert_eq!(dgx1.endpoint, "http://REDACTED-HOST:11434", "disk wins");
        assert_eq!(dgx1.effective_model(), Some("qwen3:30b"));
        assert_eq!(dgx1.kind, None, "absent kind means probe-at-connect");
        assert!(cfg.backends.iter().any(|b| b.name == "gnuc"), "gnuc kept");
    }

    #[test]
    fn dropin_probe_cache_preserves_config_declared_auth() {
        // Regression: an OpenAI-kind backend declares its bearer token in
        // config.toml (api_key_env / api_key_file). The adopt writeback persists
        // probed endpoint/model but NEVER secrets, so the drop-in carries no
        // auth. The load-merge must PRESERVE the config's auth, not clear it —
        // otherwise every session after the first adopt writeback 401s.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gpt41.toml"),
            "endpoint = \"https://api.openai.com\"\nmodel = \"gpt-4.1\"\nkind = \"openai\"\n",
        )
        .unwrap();
        let mut cfg = Config {
            backends: vec![BackendConfig {
                name: "gpt41".into(),
                endpoint: "https://api.openai.com".into(),
                model: Some("gpt-4.1".into()),
                kind: Some(BackendKind::Openai),
                api_key_env: Some("OPENAI_API_KEY".into()),
                api_key_file: Some("/vault/openai".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        cfg.merge_backends_from_dir(dir.path());
        let b = cfg.backends.iter().find(|b| b.name == "gpt41").unwrap();
        assert_eq!(b.model.as_deref(), Some("gpt-4.1"), "probed model applied");
        assert_eq!(
            b.api_key_env.as_deref(),
            Some("OPENAI_API_KEY"),
            "config-declared api_key_env must survive a keyless probe drop-in"
        );
        assert_eq!(
            b.api_key_file.as_deref(),
            Some("/vault/openai"),
            "config-declared api_key_file must survive a keyless probe drop-in"
        );
    }

    #[test]
    fn dropin_probe_cache_preserves_config_declared_tiers() {
        // Regression (2026-07-27, steering-regressions): the adopt writeback
        // records probed endpoint/model/api/serving but NOT tiers — tier
        // assignment is an operator choice, never a probed property — so the
        // drop-in carries `tiers = []`. The load-merge must PRESERVE the
        // config's tiers, not clear them. Otherwise the backend serves NO tier
        // after the first adopt writeback, and newt silently falls back to an
        // auto-discovered local backend: a live eval drive configured for a 30B
        // model on the remote router instead ran a 9B model on local ollama,
        // grinding the local GPU while the remote box sat idle.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("eval.toml"),
            "endpoint = \"http://router:8080\"\nmodel = \"big-30b\"\nkind = \"openai\"\ntiers = []\n",
        )
        .unwrap();
        let mut cfg = Config {
            backends: vec![BackendConfig {
                name: "eval".into(),
                endpoint: "http://router:8080".into(),
                model: Some("big-30b".into()),
                kind: Some(BackendKind::Openai),
                tiers: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
                ..Default::default()
            }],
            ..Default::default()
        };
        cfg.merge_backends_from_dir(dir.path());
        let b = cfg.backends.iter().find(|b| b.name == "eval").unwrap();
        assert_eq!(b.model.as_deref(), Some("big-30b"), "probed model applied");
        assert_eq!(
            b.tiers,
            vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
            "config-declared tiers must survive a probe drop-in that omits them (tiers=[])"
        );
    }

    #[test]
    fn cli_backend_override_with_endpoint_is_exclusive_and_defaults_tiers() {
        // A CLI-pinned endpoint defines the ONLY backend, discarding whatever
        // discovery/drop-ins produced (the ollama-fallback escape hatch), and
        // its tiers default to all four so it actually serves.
        let mut cfg = Config {
            backends: vec![
                BackendConfig {
                    name: "discovered-ollama".into(),
                    endpoint: "http://localhost:11434".into(),
                    kind: Some(BackendKind::Ollama),
                    tiers: vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
                    ..Default::default()
                },
                fallback_localhost_backend(),
            ],
            ..Default::default()
        };
        let over = BackendOverride {
            endpoint: Some("http://router:8080".into()),
            model: Some("big-30b".into()),
            kind: Some(BackendKind::Openai),
            ..Default::default()
        };
        over.apply(&mut cfg);
        assert_eq!(cfg.backends.len(), 1, "CLI endpoint is exclusive");
        let b = &cfg.backends[0];
        assert_eq!(b.name, "cli");
        assert_eq!(b.endpoint, "http://router:8080");
        assert_eq!(b.model.as_deref(), Some("big-30b"));
        assert_eq!(b.kind, Some(BackendKind::Openai));
        assert_eq!(
            b.tiers,
            vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review],
            "an exclusive CLI backend defaults to all tiers so it serves"
        );
    }

    #[test]
    fn cli_backend_override_field_only_edits_first_backend_in_place() {
        // With no endpoint/model_path the override is a field edit, not a new
        // backend: `--backend-model` swaps only the model of the primary backend.
        let mut cfg = Config {
            backends: vec![BackendConfig {
                name: "eval".into(),
                endpoint: "http://router:8080".into(),
                model: Some("old".into()),
                kind: Some(BackendKind::Openai),
                tiers: vec![Tier::Fast],
                ..Default::default()
            }],
            ..Default::default()
        };
        let over = BackendOverride {
            model: Some("new-model".into()),
            ..Default::default()
        };
        over.apply(&mut cfg);
        assert_eq!(cfg.backends.len(), 1, "no new backend added");
        assert_eq!(cfg.backends[0].name, "eval", "existing backend kept");
        assert_eq!(cfg.backends[0].endpoint, "http://router:8080");
        assert_eq!(cfg.backends[0].model.as_deref(), Some("new-model"));
    }

    #[test]
    fn cli_backend_override_empty_is_a_noop() {
        let mut cfg = Config {
            backends: vec![fallback_localhost_backend()],
            ..Default::default()
        };
        let before: Vec<(String, String)> = cfg
            .backends
            .iter()
            .map(|b| (b.name.clone(), b.endpoint.clone()))
            .collect();
        BackendOverride::default().apply(&mut cfg);
        let after: Vec<(String, String)> = cfg
            .backends
            .iter()
            .map(|b| (b.name.clone(), b.endpoint.clone()))
            .collect();
        assert_eq!(after, before, "an empty override changes nothing");
    }

    #[serial_test::serial(real_fs)]
    #[test]
    fn writeback_probed_backend_lands_in_dedicated_dropin_not_config_toml() {
        // Probe write-back must never touch config.toml — only backends/<name>.toml
        // so reset = delete that one file. Serial: pins NEWT_CONFIG_DIR, which
        // races any parallel test that resolves the user config dir.
        let dir = tempfile::tempdir().unwrap();
        let config_toml = dir.path().join("config.toml");
        std::fs::write(&config_toml, "# keep me\n").unwrap();
        // SAFETY: test-local env pin; restored below.
        let prev = std::env::var_os(NEWT_CONFIG_DIR_ENV);
        unsafe { std::env::set_var(NEWT_CONFIG_DIR_ENV, dir.path()) };

        let patch = BackendConfig {
            name: "dgx1-llama".into(),
            endpoint: "http://host:8000".into(),
            kind: Some(BackendKind::Openai),
            api: Some(OpenAiApi::Responses),
            model: Some("nemotron".into()),
            serving: Some(Serving::Instance),
            ..Default::default()
        };
        let written = writeback_probed_backend(&patch)
            .unwrap()
            .expect("user config dir is set");
        assert_eq!(written, dir.path().join("backends").join("dgx1-llama.toml"));
        let body = std::fs::read_to_string(&written).unwrap();
        assert!(body.contains("kind = \"openai\""));
        assert!(body.contains("api = \"responses\""));
        assert!(body.contains("model = \"nemotron\""));
        assert!(body.contains("serving = \"instance\""));
        // Main config untouched.
        assert_eq!(
            std::fs::read_to_string(&config_toml).unwrap(),
            "# keep me\n"
        );

        // Second write merges and preserves auth refs already on disk.
        std::fs::write(
            &written,
            "endpoint = \"http://host:8000\"\napi_key_env = \"DGX_TOKEN\"\n",
        )
        .unwrap();
        let patch2 = BackendConfig {
            name: "dgx1-llama".into(),
            endpoint: "http://host:8000".into(),
            kind: Some(BackendKind::Openai),
            api: Some(OpenAiApi::ChatCompletions),
            model: Some("other".into()),
            ..Default::default()
        };
        writeback_probed_backend(&patch2).unwrap();
        let body2 = std::fs::read_to_string(&written).unwrap();
        assert!(body2.contains("api_key_env"), "auth ref preserved: {body2}");
        assert!(body2.contains("api = \"chat_completions\""));
        assert!(body2.contains("model = \"other\""));

        match prev {
            Some(v) => unsafe { std::env::set_var(NEWT_CONFIG_DIR_ENV, v) },
            None => unsafe { std::env::remove_var(NEWT_CONFIG_DIR_ENV) },
        }
    }

    #[test]
    fn disk_dgx_nodes_load_per_file_by_stem_and_override_inline() {
        let dir = tempfile::tempdir().unwrap();
        // A minimal drop-in: name omitted (filename is authoritative), carries
        // the multi-endpoint info a [[backends]] entry can't (vllm + ssh_host).
        std::fs::write(
            dir.path().join("dgx1.toml"),
            "ollama = \"http://REDACTED-HOST:11434\"\n\
             vllm = \"http://REDACTED-HOST:8000\"\n\
             ssh_host = \"REDACTED-HOST\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "not a node").unwrap();

        // [dgx] absent → created on first drop-in, with the node populated.
        let mut cfg = Config::default();
        assert!(cfg.dgx.is_none());
        cfg.merge_dgx_nodes_from_dir(dir.path());
        let dgx = cfg.dgx.as_ref().expect("[dgx] created from drop-ins");
        assert_eq!(dgx.nodes.len(), 1);
        let node = &dgx.nodes[0];
        assert_eq!(node.name, "dgx1", "name comes from the filename stem");
        assert_eq!(node.ollama.as_deref(), Some("http://REDACTED-HOST:11434"));
        assert_eq!(node.vllm.as_deref(), Some("http://REDACTED-HOST:8000"));
        assert_eq!(node.ssh_host.as_deref(), Some("REDACTED-HOST"));
        // A single node resolves as active without an explicit active_node.
        assert_eq!(dgx.active_node().unwrap().name, "dgx1");

        // Disk replaces an inline node of the same name in place (no duplicate).
        cfg.dgx.as_mut().unwrap().nodes[0].ollama = Some("http://stale:1".into());
        cfg.merge_dgx_nodes_from_dir(dir.path());
        assert_eq!(cfg.dgx.as_ref().unwrap().nodes.len(), 1, "no duplicate");
        assert_eq!(
            cfg.dgx.unwrap().nodes[0].ollama.as_deref(),
            Some("http://REDACTED-HOST:11434"),
            "disk wins"
        );
    }

    #[test]
    fn backendless_config_deserializes_empty_but_default_keeps_fallback() {
        // A config.toml with no [[backends]] must NOT inherit the struct-default
        // localhost Ollama — otherwise a drop-in-only setup gets a spurious
        // 'ollama' entry alongside its real backends (the migration regression).
        let cfg: Config = toml::from_str("providers = []\n").unwrap();
        assert!(
            cfg.backends.is_empty(),
            "absent [[backends]] deserializes to empty, got {:?}",
            cfg.backends
        );
        // But the no-config-file path (Config::default) keeps the fallback.
        assert_eq!(Config::default().backends.len(), 1);
        assert_eq!(Config::default().backends[0].name, "ollama");
        // Inline backends still load normally.
        let inline: Config =
            toml::from_str("[[backends]]\nname=\"x\"\nendpoint=\"http://h:1\"\nmodel=\"m\"\n")
                .unwrap();
        assert_eq!(inline.backends.len(), 1);
        assert_eq!(inline.backends[0].name, "x");
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

    /// Serial: reads `user_config_dir()`, which honors NEWT_CONFIG_DIR — a
    /// parallel serial-lane test pinning that var to a tempdir makes the
    /// `.newt` parent assertion observe the tempdir instead (caught by the
    /// slower Windows CI runner).
    #[serial_test::serial(real_fs)]
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

    /// #1021 PR 5.2: `personas_dir()` is the sibling-of-config default
    /// `PersonaStore::default_dir()` (newt-tui) also resolves to — a headless
    /// caller gets the exact same location without depending on newt-tui.
    #[serial_test::serial(real_fs)] // same NEWT_CONFIG_DIR-reader race as above
    #[test]
    fn personas_dir_is_a_sibling_of_the_newt_config_dir() {
        let dir = Config::personas_dir();
        assert!(dir.ends_with("personas"));
        assert_eq!(
            dir.parent().and_then(|p| p.file_name()),
            Some(".newt".as_ref())
        );
    }

    #[test]
    fn skill_search_dirs_preserves_configured_order() {
        let cfg = Config {
            skills: Some(SkillsConfig {
                search: vec!["/abs/one".into(), "/abs/two".into()],
                bundled_dir: String::new(),
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
                bundled_dir: String::new(),
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
    fn skill_search_dirs_appends_bundled_dir_last() {
        // Bundled dir is LOWEST priority: user `search` paths come first so a
        // user skill of the same name wins the collision (earlier dirs win in
        // `discover_paths`), and the bundled dir is appended last.
        let cfg = Config {
            skills: Some(SkillsConfig {
                search: vec!["/abs/user".into()],
                bundled_dir: "/abs/bundled".into(),
            }),
            ..Config::default()
        };
        assert_eq!(
            cfg.skill_search_dirs(),
            vec![PathBuf::from("/abs/user"), PathBuf::from("/abs/bundled")],
            "user search dirs must precede the bundled dir so users can override"
        );
    }

    #[test]
    fn skill_search_dirs_bundled_after_default_when_search_empty() {
        // No `search` configured: the host default (`~/.newt/skills`) still
        // precedes the bundled dir. An empty `bundled_dir` adds nothing.
        let with_bundled = Config {
            skills: Some(SkillsConfig {
                search: vec![],
                bundled_dir: "/abs/bundled".into(),
            }),
            ..Config::default()
        };
        let dirs = with_bundled.skill_search_dirs();
        assert_eq!(dirs.len(), 2, "default host dir + bundled: {dirs:?}");
        assert!(
            dirs[0].ends_with("skills"),
            "default host dir first: {dirs:?}"
        );
        assert_eq!(
            dirs[1],
            PathBuf::from("/abs/bundled"),
            "bundled last: {dirs:?}"
        );

        let no_bundled = Config {
            skills: Some(SkillsConfig {
                search: vec![],
                bundled_dir: String::new(),
            }),
            ..Config::default()
        };
        assert_eq!(
            no_bundled.skill_search_dirs().len(),
            1,
            "empty bundled_dir contributes no directory"
        );
    }

    #[test]
    fn find_ancestor_dir_returns_first_matching_ancestor() {
        // Only the workspace root has `.newt/bundled-skills`; the walk from a
        // nested cwd must find it there, not stop short or overshoot.
        let root = Path::new("/home/u/repo");
        let target = root.join(".newt/bundled-skills");
        let exists = |p: &Path| p == target;
        let got = find_ancestor_dir(
            Path::new("/home/u/repo/newt-core/src"),
            Path::new(".newt/bundled-skills"),
            exists,
        );
        assert_eq!(got, Some(target));
    }

    #[test]
    fn find_ancestor_dir_none_when_no_ancestor_has_it() {
        let got = find_ancestor_dir(
            Path::new("/home/u/repo/newt-core/src"),
            Path::new(".newt/bundled-skills"),
            |_| false,
        );
        assert_eq!(got, None, "no ancestor matches → None, never a bogus path");
    }

    #[test]
    fn with_bundled_default_leaves_a_configured_value_untouched() {
        // A user who set `bundled_dir` must win — the checkout default only
        // fills the gap, it never overrides an explicit choice.
        let cfg = Config {
            skills: Some(SkillsConfig {
                search: vec![],
                bundled_dir: "/explicit/bundled".into(),
            }),
            ..Config::default()
        }
        .with_bundled_default();
        assert_eq!(
            cfg.skills.unwrap().bundled_dir,
            "/explicit/bundled",
            "an explicitly configured bundled_dir is never overridden"
        );
    }

    #[test]
    fn skills_search_round_trips_through_toml() {
        let cfg = Config {
            skills: Some(SkillsConfig {
                search: vec!["~/.newt/skills".into(), "~/.claude/skills".into()],
                bundled_dir: String::new(),
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
        // #1030: fresh-on-launch — auto-resume defaults OFF now; `resume = true`
        // is the opt-in back to auto-resuming the folder's latest conversation.
        assert!(!conversations.resume);
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
        // Partial [conversations] table: unset keys keep their defaults
        // (#1030: `resume` now defaults false = fresh-on-launch).
        assert!(!conversations.resume);
    }

    #[test]
    fn conversations_resume_opt_in_parses() {
        // #1030: `resume = true` opts back into auto-resuming the folder's
        // latest conversation (the pre-#1030 default, now off by default).
        let cfg: Config = toml::from_str(
            r#"
[conversations]
resume = true
"#,
        )
        .unwrap();

        assert!(cfg.conversations.unwrap_or_default().resume);
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
        assert_eq!(cfg.backends[0].effective_model(), Some("mistral:7b"));
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

    #[serial_test::serial(real_fs)]
    #[test]
    fn resolve_returns_default_when_no_file() {
        // Use a temp dir as cwd and clear env to ensure no candidates match.
        // Serial: mutates process-global cwd + HOME, which races any parallel
        // test that resolves paths (the unconfigured-provenance test shares
        // this lane for the same reason).
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
        assert!(
            cfg.is_unconfigured(),
            "a resolve with no config anywhere is the unboxing state"
        );
    }

    #[test]
    fn default_config_is_unconfigured() {
        assert!(
            Config::default().is_unconfigured(),
            "the struct default's sole backend is the compiled-in fallback"
        );
    }

    #[test]
    fn dropin_merge_clears_the_unconfigured_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gpu.toml"),
            "endpoint = \"http://gpu:11434\"\n",
        )
        .unwrap();
        let mut cfg = Config::default();
        assert!(cfg.is_unconfigured());
        cfg.merge_backends_from_dir(dir.path());
        assert!(
            !cfg.is_unconfigured(),
            "a successfully merged drop-in is operator configuration"
        );
    }

    #[test]
    fn skipped_and_malformed_dropins_do_not_clear_the_unconfigured_flag() {
        let dir = tempfile::tempdir().unwrap();
        // Malformed TOML → warn + skip.
        std::fs::write(dir.path().join("bad.toml"), "endpoint = 42\n").unwrap();
        // No endpoint and no model_path → skipped by the destination check.
        std::fs::write(dir.path().join("hollow.toml"), "model = \"m\"\n").unwrap();
        let mut cfg = Config::default();
        cfg.merge_backends_from_dir(dir.path());
        assert!(
            cfg.is_unconfigured(),
            "only a drop-in that actually merges counts as configuration"
        );
    }

    #[test]
    fn cli_backend_override_clears_the_unconfigured_flag() {
        let mut cfg = Config::default();
        BackendOverride {
            model: Some("qwen3:32b".into()),
            ..Default::default()
        }
        .apply(&mut cfg);
        assert!(
            !cfg.is_unconfigured(),
            "an explicit --backend-* flag is operator configuration"
        );
        // …but an empty override stays a no-op.
        let mut untouched = Config::default();
        BackendOverride::default().apply(&mut untouched);
        assert!(untouched.is_unconfigured());
    }

    /// `resolve()`-boundary provenance: inline `[[backends]]` in a config file
    /// and `backends/*.toml` drop-ins both mean "configured"; a config file
    /// that declares neither is as bare as no file at all. Serial: pins
    /// NEWT_CONFIG_DIR / HOME / cwd like `resolve_returns_default_when_no_file`.
    #[serial_test::serial(real_fs)]
    #[test]
    fn resolve_reports_unconfigured_only_without_operator_backends() {
        let dir = tempfile::tempdir().unwrap();
        let saved_config = std::env::var_os("NEWT_CONFIG");
        let saved_config_dir = std::env::var_os(NEWT_CONFIG_DIR_ENV);
        let saved_home = std::env::var_os("HOME");
        std::env::remove_var("NEWT_CONFIG");
        std::env::set_var(NEWT_CONFIG_DIR_ENV, dir.path());
        std::env::set_var("HOME", dir.path());
        let prev_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir.path()).unwrap();

        let config_toml = dir.path().join("config.toml");

        // 1. Config file with no backends and no drop-ins → still unboxed.
        std::fs::write(&config_toml, "providers = []\n").unwrap();
        let bare = Config::resolve().unwrap();

        // 2. Inline [[backends]] → configured.
        std::fs::write(
            &config_toml,
            "[[backends]]\nname = \"gpu\"\nendpoint = \"http://gpu:8000\"\n",
        )
        .unwrap();
        let inline = Config::resolve().unwrap();

        // 3. Backend-less config file + a drop-in → configured.
        std::fs::write(&config_toml, "providers = []\n").unwrap();
        std::fs::create_dir_all(dir.path().join("backends")).unwrap();
        std::fs::write(
            dir.path().join("backends").join("gpu.toml"),
            "endpoint = \"http://gpu:11434\"\n",
        )
        .unwrap();
        let dropin = Config::resolve().unwrap();

        std::env::set_current_dir(prev_dir).unwrap();
        match saved_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match saved_config_dir {
            Some(v) => std::env::set_var(NEWT_CONFIG_DIR_ENV, v),
            None => std::env::remove_var(NEWT_CONFIG_DIR_ENV),
        }
        if let Some(v) = saved_config {
            std::env::set_var("NEWT_CONFIG", v);
        }

        assert!(
            bare.is_unconfigured(),
            "a backend-less config file is as bare as no file"
        );
        assert!(!inline.is_unconfigured(), "inline [[backends]] configure");
        assert!(!dropin.is_unconfigured(), "a drop-in configures");
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

    // --- #1301: project-origin `[[mcp_servers]]` are stamped UNTRUSTED ---

    /// A minimal valid stdio entry at the `#[serde(skip)]` default trust
    /// ([`crate::mcp::McpTrust::Trusted`]) — mirrors a freshly-deserialized entry.
    fn mcp_entry(name: &str) -> crate::mcp::McpServerEntry {
        crate::mcp::McpServerEntry {
            name: name.into(),
            enabled: true,
            transport: crate::mcp::TransportKind::Stdio,
            command: Some("true".into()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            request_timeout_secs: None,
            trust: crate::mcp::McpTrust::Trusted,
        }
    }

    #[test]
    fn mark_project_mcp_untrusted_replace_marks_every_entry() {
        // Replace (the default) with a project `mcp_servers` array present: the
        // project array REPLACED the base's, so every survivor is project-origin.
        let mut servers = vec![mcp_entry("a"), mcp_entry("b")];
        mark_project_mcp_untrusted(&mut servers, ArrayMergeStrategy::Replace, Some(2));
        assert!(
            servers
                .iter()
                .all(|e| e.trust == crate::mcp::McpTrust::Untrusted),
            "replace ⇒ all project-origin ⇒ all untrusted"
        );
    }

    #[test]
    fn mark_project_mcp_untrusted_append_marks_only_trailing_project_entries() {
        // Append: base entries first, project entries appended — only the
        // trailing `count` (here 2) are project-origin.
        let mut servers = vec![mcp_entry("base"), mcp_entry("p1"), mcp_entry("p2")];
        mark_project_mcp_untrusted(&mut servers, ArrayMergeStrategy::Append, Some(2));
        assert_eq!(
            servers[0].trust,
            crate::mcp::McpTrust::Trusted,
            "the trusted base entry must stay trusted"
        );
        assert_eq!(servers[1].trust, crate::mcp::McpTrust::Untrusted);
        assert_eq!(servers[2].trust, crate::mcp::McpTrust::Untrusted);
    }

    #[test]
    fn mark_project_mcp_untrusted_none_marks_nothing() {
        // No project `mcp_servers` key ⇒ the array came wholly from the trusted
        // base (user config) ⇒ nothing is downgraded.
        let mut servers = vec![mcp_entry("a")];
        mark_project_mcp_untrusted(&mut servers, ArrayMergeStrategy::Replace, None);
        assert_eq!(servers[0].trust, crate::mcp::McpTrust::Trusted);
    }

    #[test]
    fn base_is_ambient_newt_toml_false_for_non_newt_toml_base() {
        // A base that isn't the cwd `./newt.toml` candidate is never ambient,
        // regardless of `$NEWT_CONFIG` — the user home config, `/etc`, and an
        // explicit non-`newt.toml` base all stay trusted. (The env-dependent
        // `./newt.toml` branches are covered end-to-end in
        // tests/mcp_project_trust.rs, which controls `$NEWT_CONFIG`.)
        assert!(!base_is_ambient_newt_toml(None));
        assert!(!base_is_ambient_newt_toml(Some(Path::new(
            "/etc/newt/config.toml"
        ))));
        assert!(!base_is_ambient_newt_toml(Some(Path::new(
            "./newt-other.toml"
        ))));
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
    fn to_redacted_toml_redacts_literals_but_keeps_secret_references() {
        // A literal secret AND a `${cmd:…}` interpolation literal are both
        // redacted (a literal can embed raw secret text); a `{ cmd = … }`
        // SecretRef is a REFERENCE — it names where the secret lives, not the
        // secret — so it is kept, exactly like `api_key_file`.
        let cfg: Config = toml::from_str(
            r#"
            [[mcp_servers]]
            name = "gh"
            command = "gh-mcp"
            [mcp_servers.env]
            RAW = "ghp_rawinlinesecret"
            VAULTED = { cmd = "vault kv get -field=token secret/data/gh" }
            [mcp_servers.headers]
            Authorization = "Bearer ${cmd:vault kv get -field=token secret/data/api}"
            "#,
        )
        .unwrap();

        let dump = cfg.to_redacted_toml().unwrap();
        // Literal secret and the interpolation literal never appear…
        assert!(
            !dump.contains("ghp_rawinlinesecret"),
            "raw secret leaked:\n{dump}"
        );
        assert!(
            !dump.contains("secret/data/api"),
            "interpolation literal leaked:\n{dump}"
        );
        assert!(dump.contains(Config::REDACTED));
        // …but the KEYS survive, and the SecretRef reference is kept (it names
        // a location, not a secret) — the operator can audit their wiring.
        assert!(dump.contains("RAW"));
        assert!(dump.contains("VAULTED"));
        assert!(dump.contains("Authorization"));
        assert!(
            dump.contains("vault kv get -field=token secret/data/gh"),
            "SecretRef reference kept:\n{dump}"
        );
    }

    #[test]
    fn to_redacted_toml_redacts_url_userinfo_query_and_args() {
        // FIX 5 (#1301): url and args are plain strings (no SecretValue wrapper),
        // so URL-embedded creds and `--token` args must be redacted before the
        // audit dump can leak them.
        let cfg: Config = toml::from_str(
            r#"
            [[mcp_servers]]
            name = "gh"
            type = "http"
            url = "https://alice:sk-URLPASS@api.example/mcp?api_key=SECRETQP&region=us"
            args = ["--token=sk-EQ", "--api-key", "sk-SPACE", "--verbose"]
            "#,
        )
        .unwrap();
        let dump = cfg.to_redacted_toml().unwrap();
        // None of the secret material survives…
        for leaked in ["sk-URLPASS", "SECRETQP", "sk-EQ", "sk-SPACE", "alice"] {
            assert!(!dump.contains(leaked), "`{leaked}` leaked:\n{dump}");
        }
        // …but the non-secret structure does.
        assert!(dump.contains("api.example/mcp"), "host/path kept:\n{dump}");
        assert!(dump.contains("region=us"), "non-secret param kept:\n{dump}");
        assert!(dump.contains("--verbose"), "non-secret arg kept:\n{dump}");
        assert!(dump.contains(Config::REDACTED));
    }

    #[test]
    fn redact_url_and_args_helpers_are_precise() {
        // Userinfo + sensitive query param redacted; scheme/host/path/fragment
        // and a non-sensitive param preserved.
        assert_eq!(
            redact_url_secrets("https://u:p@h.example/mcp?token=abc&keep=1#frag"),
            format!(
                "https://{r}@h.example/mcp?token={r}&keep=1#frag",
                r = Config::REDACTED
            )
        );
        // No userinfo, no sensitive params → unchanged.
        assert_eq!(
            redact_url_secrets("https://h.example/mcp?region=us"),
            "https://h.example/mcp?region=us"
        );
        // An `@` in the path is not userinfo.
        assert_eq!(
            redact_url_secrets("https://h.example/a@b"),
            "https://h.example/a@b"
        );
        // Both arg forms; a non-sensitive flag with a value is untouched.
        assert_eq!(
            redact_arg_secrets(&[
                "--token=sk-1".into(),
                "--api-key".into(),
                "sk-2".into(),
                "--model".into(),
                "gpt".into(),
            ]),
            vec![
                format!("--token={}", Config::REDACTED),
                "--api-key".to_string(),
                Config::REDACTED.to_string(),
                "--model".to_string(),
                "gpt".to_string(),
            ]
        );
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

    // --- #1292: the shared MCP probe leash (doctor + `newt mcp probe`) ---

    #[test]
    fn mcp_probe_caveats_default_is_read_only_never_top() {
        let cav = Config::default().mcp_probe_caveats(std::path::Path::new("/workspace"));
        assert!(cav.permits_fs_read("/workspace/src/main.rs"));
        assert!(
            !cav.permits_fs_write("/workspace/src/main.rs"),
            "unconfigured probe leash must not write"
        );
        assert!(
            !cav.permits_exec("cargo"),
            "unconfigured probe leash grants no exec (the spawn path widens \
             exactly the probed command, nothing else)"
        );
    }

    #[test]
    fn mcp_probe_caveats_honors_the_configured_preset() {
        let cfg = Config {
            tui: Some(TuiConfig {
                permissions: ToolPermissions::default(), // WorkspaceDev
                ..Default::default()
            }),
            ..Default::default()
        };
        let cav = cfg.mcp_probe_caveats(std::path::Path::new("/ws"));
        assert!(cav.permits_exec("cargo"), "configured preset respected");
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

    // ---- #1149: /mcp enable|disable config writer ----

    #[test]
    fn with_mcp_enabled_toggles_and_preserves_comments() {
        let text = "# my config\n[[mcp_servers]]\nname = \"modulex\"\ncommand = \"modulex-mcp\"\n";
        // disable → enabled = false written, comment preserved
        let off = Config::with_mcp_enabled(text, "modulex", false).unwrap();
        assert!(off.contains("enabled = false"));
        assert!(off.contains("# my config"));
        // re-enable → key REMOVED (default is enabled; file stays minimal)
        let on = Config::with_mcp_enabled(&off, "modulex", true).unwrap();
        assert!(!on.contains("enabled"));
        // unknown name errors loudly
        assert!(Config::with_mcp_enabled(text, "nope", false).is_err());
        // entry parses with default enabled=true; explicit false honored
        let e: crate::mcp::McpServerEntry =
            toml::from_str("name = \"x\"\ncommand = \"x\"\n").unwrap();
        assert!(e.enabled);
        let d: crate::mcp::McpServerEntry =
            toml::from_str("name = \"x\"\ncommand = \"x\"\nenabled = false\n").unwrap();
        assert!(!d.enabled);
    }

    // ---- `newt mcp add|remove` comment-preserving config writers ----

    #[test]
    fn with_mcp_server_added_appends_and_preserves_comments() {
        let text = "\
# hand-authored config
default_backend = \"local\" # keep me

[[mcp_servers]]
name = \"modulex\"
command = \"modulex-mcp\"
";
        let entry = crate::mcp::McpServerEntry {
            name: "scrybe".into(),
            enabled: true,
            transport: crate::mcp::TransportKind::Stdio,
            command: Some("scrybe-mcp-server".into()),
            args: vec!["stdio".into()],
            env: std::collections::BTreeMap::from([(
                "SCRYBE_LOG".to_string(),
                crate::mcp::SecretValue::literal("info"),
            )]),
            url: None,
            headers: std::collections::BTreeMap::new(),
            request_timeout_secs: Some(120),
            trust: crate::mcp::McpTrust::Trusted,
        };
        let out = Config::with_mcp_server_added(text, &entry).unwrap();
        assert!(
            out.contains("# hand-authored config"),
            "comment lost: {out}"
        );
        assert!(out.contains("# keep me"), "inline comment lost: {out}");
        assert!(out.contains("modulex-mcp"), "existing entry lost: {out}");
        // Round-trips through the typed config with both entries intact.
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 2);
        let added = cfg.mcp_servers.iter().find(|s| s.name == "scrybe").unwrap();
        assert_eq!(added.command.as_deref(), Some("scrybe-mcp-server"));
        assert_eq!(added.args, vec!["stdio"]);
        assert_eq!(
            added
                .env
                .get("SCRYBE_LOG")
                .and_then(crate::mcp::SecretValue::as_literal),
            Some("info")
        );
        assert_eq!(added.request_timeout_secs, Some(120));
        assert!(added.enabled);
        // Defaults stay implicit — the file stays minimal.
        assert!(!out.contains("enabled"), "default enabled written: {out}");
        assert!(!out.contains("type"), "default transport written: {out}");
    }

    #[test]
    fn with_mcp_server_added_creates_section_in_empty_text() {
        let entry = crate::mcp::McpServerEntry {
            name: "fs".into(),
            enabled: true,
            transport: crate::mcp::TransportKind::Stdio,
            command: Some("mcp-fs".into()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            request_timeout_secs: None,
            trust: crate::mcp::McpTrust::Trusted,
        };
        let out = Config::with_mcp_server_added("", &entry).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].name, "fs");
        assert_eq!(cfg.mcp_servers[0].command.as_deref(), Some("mcp-fs"));
    }

    #[test]
    fn with_mcp_server_added_writes_sse_transport_and_url() {
        let entry = crate::mcp::McpServerEntry {
            name: "remote".into(),
            enabled: true,
            transport: crate::mcp::TransportKind::Sse,
            command: None,
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: Some("https://mcp.example/sse".into()),
            headers: std::collections::BTreeMap::new(),
            request_timeout_secs: None,
            trust: crate::mcp::McpTrust::Trusted,
        };
        let out = Config::with_mcp_server_added("", &entry).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.mcp_servers[0].transport, crate::mcp::TransportKind::Sse);
        assert_eq!(
            cfg.mcp_servers[0].url.as_deref(),
            Some("https://mcp.example/sse")
        );
    }

    #[test]
    fn with_mcp_server_added_rejects_duplicates_and_invalid_entries() {
        let text = "[[mcp_servers]]\nname = \"scrybe\"\ncommand = \"scrybe-mcp-server\"\n";
        let dup = crate::mcp::McpServerEntry {
            name: "scrybe".into(),
            enabled: true,
            transport: crate::mcp::TransportKind::Stdio,
            command: Some("other".into()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            request_timeout_secs: None,
            trust: crate::mcp::McpTrust::Trusted,
        };
        let err = Config::with_mcp_server_added(text, &dup).unwrap_err();
        assert!(err.to_string().contains("scrybe"), "names the dup: {err}");

        // A stdio entry with no command / an sse entry with no url never lands
        // in the file — it could never connect (mcp::McpServerEntry::is_valid).
        let no_cmd = crate::mcp::McpServerEntry {
            name: "ghost".into(),
            command: None,
            ..dup.clone()
        };
        assert!(Config::with_mcp_server_added("", &no_cmd).is_err());
        let no_url = crate::mcp::McpServerEntry {
            name: "ghost".into(),
            transport: crate::mcp::TransportKind::Http,
            command: None,
            ..dup.clone()
        };
        assert!(Config::with_mcp_server_added("", &no_url).is_err());
        // An empty name can never be addressed again — reject it.
        let unnamed = crate::mcp::McpServerEntry {
            name: "  ".into(),
            ..dup.clone()
        };
        assert!(Config::with_mcp_server_added("", &unnamed).is_err());
    }

    #[test]
    fn with_mcp_server_removed_deletes_only_the_named_entry() {
        let text = "\
# my config

[[mcp_servers]]
name = \"keep\"
command = \"keep-mcp\" # keep note

[[mcp_servers]]
name = \"drop\"
command = \"drop-mcp\"
";
        let out = Config::with_mcp_server_removed(text, "drop").unwrap();
        assert!(out.contains("# my config"), "comment lost: {out}");
        assert!(out.contains("# keep note"), "inline comment lost: {out}");
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].name, "keep");
        assert!(!out.contains("drop-mcp"));
    }

    #[test]
    fn with_mcp_server_removed_reports_a_non_array_section_accurately() {
        // The inline-array form is valid TOML the serde reader accepts; the
        // writer must say it cannot edit that shape, not falsely claim the
        // entry is absent.
        let text = "mcp_servers = [ { name = \"x\", command = \"y\" } ]\n";
        let err = Config::with_mcp_server_removed(text, "x").unwrap_err();
        assert!(
            err.to_string().contains("not an array of tables"),
            "wrong-shape section misreported: {err}"
        );
        let err = Config::with_mcp_server_removed("mcp_servers = 3\n", "x").unwrap_err();
        assert!(
            err.to_string().contains("not an array of tables"),
            "scalar section misreported: {err}"
        );
    }

    #[test]
    fn mcp_writer_error_branches_are_loud() {
        let entry = crate::mcp::McpServerEntry {
            name: "x".into(),
            enabled: true,
            transport: crate::mcp::TransportKind::Stdio,
            command: Some("x-mcp".into()),
            args: vec![],
            env: std::collections::BTreeMap::new(),
            url: None,
            headers: std::collections::BTreeMap::new(),
            request_timeout_secs: None,
            trust: crate::mcp::McpTrust::Trusted,
        };
        // Invalid TOML input text.
        let err = Config::with_mcp_server_added("not toml [", &entry).unwrap_err();
        assert!(err.to_string().contains("not valid TOML"), "{err}");
        let err = Config::with_mcp_server_removed("not toml [", "x").unwrap_err();
        assert!(err.to_string().contains("not valid TOML"), "{err}");
        // A section that is not an array of tables.
        let err = Config::with_mcp_server_added("mcp_servers = 3\n", &entry).unwrap_err();
        assert!(err.to_string().contains("not an array of tables"), "{err}");
        // A timeout that does not fit TOML's i64 integers.
        let oversized = crate::mcp::McpServerEntry {
            request_timeout_secs: Some(u64::MAX),
            ..entry
        };
        let err = Config::with_mcp_server_added("", &oversized).unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn with_mcp_server_removed_errors_when_absent() {
        let text = "[[mcp_servers]]\nname = \"present\"\ncommand = \"x\"\n";
        let err = Config::with_mcp_server_removed(text, "ghost").unwrap_err();
        assert!(err.to_string().contains("ghost"), "names the miss: {err}");
        // No section at all errors the same way, not a panic.
        assert!(Config::with_mcp_server_removed("", "ghost").is_err());
    }

    // ---- comment-preserving default-backend writer ----

    #[test]
    fn with_default_backend_updates_value_and_preserves_unrelated_content() {
        let original = "\
# hand-authored config
default_backend = \"old\" # keep this selection note

[discovery]
hosts = [\"localhost\", \"dgx1.home.lab\"]

[custom]
operator_note = \"leave me alone\" # custom inline comment
";

        let out = Config::with_default_backend(original, "dgx1-openai-8000").unwrap();
        let parsed: toml::Value = toml::from_str(&out).unwrap();

        assert_eq!(
            parsed.get("default_backend").and_then(toml::Value::as_str),
            Some("dgx1-openai-8000")
        );
        assert!(
            out.contains("# hand-authored config"),
            "top comment lost: {out}"
        );
        assert!(
            out.contains("# keep this selection note"),
            "target inline comment lost: {out}"
        );
        assert!(
            out.contains("dgx1.home.lab"),
            "discovery table changed: {out}"
        );
        assert!(
            out.contains("leave me alone"),
            "custom table changed: {out}"
        );
        assert!(
            out.contains("# custom inline comment"),
            "unrelated inline comment lost: {out}"
        );
    }

    #[test]
    fn with_default_backend_creates_key_and_is_idempotent() {
        let original = "# config without a default\n[discovery]\nhosts = [\"localhost\"]\n";
        let once = Config::with_default_backend(original, "local").unwrap();
        let twice = Config::with_default_backend(&once, "local").unwrap();

        let parsed: toml::Value = toml::from_str(&once).unwrap();
        assert_eq!(
            parsed.get("default_backend").and_then(toml::Value::as_str),
            Some("local")
        );
        assert_eq!(twice, once, "reapplying the same default changed output");
        assert_eq!(twice.matches("default_backend").count(), 1);
    }

    #[test]
    fn with_default_backend_rejects_empty_name() {
        assert!(Config::with_default_backend("", "").is_err());
        assert!(Config::with_default_backend("", "   ").is_err());
    }

    #[test]
    fn with_default_backend_rejects_invalid_toml() {
        assert!(Config::with_default_backend("this = = not toml", "local").is_err());
    }

    // ---- #904: comment-preserving "allow permanently" net writer ----

    #[test]
    fn with_net_host_creates_table_from_empty_and_scope_includes_host() {
        let out = Config::with_net_host("", "github.com").unwrap();
        // The written TOML parses back and its net scope now permits the host.
        let cfg: Config = toml::from_str(&out).unwrap();
        let perms = cfg.tui.unwrap().permissions;
        assert!(perms.net.contains(&"github.com".to_string()));
        assert!(
            matches!(perms.net_scope(), crate::caveats::Scope::Only(ref s) if s.contains("github.com")),
            "net_scope must permit the granted host"
        );
    }

    #[test]
    fn with_net_host_preserves_comments_and_other_keys() {
        let original = "\
# my hand-authored config — keep this comment
[tui.permissions]
preset = \"workspace_dev\"  # inline comment
net = [\"already.example.com\"]
";
        let out = Config::with_net_host(original, "github.com").unwrap();
        // Comments survive (the whole point vs Config::save).
        assert!(
            out.contains("# my hand-authored config"),
            "top comment lost: {out}"
        );
        assert!(
            out.contains("# inline comment"),
            "inline comment lost: {out}"
        );
        // The pre-existing host is kept and the new one appended.
        assert!(out.contains("already.example.com"));
        assert!(out.contains("github.com"));
        // preset key untouched.
        assert!(out.contains("workspace_dev"));
    }

    #[test]
    fn with_net_host_is_idempotent_no_duplicate() {
        let once = Config::with_net_host("", "github.com").unwrap();
        let twice = Config::with_net_host(&once, "github.com").unwrap();
        assert_eq!(
            twice.matches("github.com").count(),
            1,
            "duplicated host: {twice}"
        );
    }

    #[test]
    fn with_net_host_rejects_invalid_toml() {
        assert!(Config::with_net_host("this = = not toml", "github.com").is_err());
    }

    fn openai_backend(api_key_file: Option<String>, api_key_env: Option<String>) -> BackendConfig {
        BackendConfig {
            name: "remote".into(),
            endpoint: "https://example.test".into(),
            model: Some("some-model".into()),
            model_path: None,
            tiers: vec![Tier::Fast],
            kind: Some(BackendKind::Openai),
            api: Default::default(),
            api_key_file,
            api_key_env,
            ..Default::default()
        }
    }

    #[test]
    fn backend_kind_absent_means_probe_at_connect() {
        let toml = r#"
            [[backends]]
            name = "local"
            endpoint = "http://localhost:8000"
            model = "m"
            tiers = ["FAST"]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.backends[0].kind, None);
        assert!(cfg.backends[0].needs_kind_probe());
        assert_eq!(cfg.backends[0].kind_label(), "auto");
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
            assert_eq!(
                cfg.backends[0].kind,
                Some(BackendKind::Openai),
                "kind={kind_str}"
            );
        }
    }

    #[test]
    fn backend_kind_label_is_protocol_name() {
        assert_eq!(BackendKind::Ollama.label(), "ollama");
        assert_eq!(BackendKind::Openai.label(), "openai");
    }

    #[test]
    fn backend_config_roundtrips_auth_fields() {
        let cfg = openai_backend(Some("~/.newt/token".into()), Some("MY_TOKEN".into()));
        let toml = toml::to_string(&cfg).unwrap();
        assert!(toml.contains("kind = \"openai\""));
        assert!(toml.contains("api_key_file"));
        assert!(toml.contains("api_key_env"));
        let back: BackendConfig = toml::from_str(&toml).unwrap();
        assert_eq!(back.kind, Some(BackendKind::Openai));
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

    /// #1235: the spill-view height defaults to 3, parses when absent, and
    /// overrides from `[tui]`.
    #[test]
    fn spill_lines_defaults_to_3_and_overrides() {
        assert_eq!(TuiConfig::default().spill_lines, 3);
        let empty: TuiConfig = toml::from_str("").unwrap();
        assert_eq!(empty.spill_lines, 3);
        let set: TuiConfig = toml::from_str("spill_lines = 7").unwrap();
        assert_eq!(set.spill_lines, 7);
    }

    #[test]
    fn default_max_tool_rounds_is_40() {
        // #<issue>: raised from 25 — a modest safety margin alongside
        // workflow_grace_rounds and the diagnose_failure delegate hint, not a
        // substitute for either. The function default and the struct default
        // agree on 40.
        assert_eq!(default_max_tool_rounds(), 40);
        assert_eq!(TuiConfig::default().max_tool_rounds, 40);
        assert_eq!(default_workflow_grace_rounds(), 5);
        assert_eq!(TuiConfig::default().workflow_grace_rounds, 5);
    }

    #[test]
    fn tui_max_tool_rounds_defaults_when_field_absent() {
        // An empty `[tui]` table => serde default kicks in => 40.
        let toml = r#"
            [tui]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.tui.unwrap().max_tool_rounds, 40);
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
    fn tui_narration_nudge_cap_defaults_to_one_and_can_be_raised() {
        // Lever L3 (next-loop-levers.md): the narrate-then-stop rescue budget
        // is config, not a hardcoded const. Default 1 preserves the historical
        // behavior; the function default and the struct default agree.
        assert_eq!(default_narration_nudge_cap(), 1);
        assert_eq!(TuiConfig::default().narration_nudge_cap, 1);

        // An empty `[tui]` table => serde default kicks in => 1.
        let cfg: Config = toml::from_str("[tui]\n").unwrap();
        assert_eq!(cfg.tui.unwrap().narration_nudge_cap, 1);

        // Weak-local-model operators raise it.
        let cfg: Config = toml::from_str("[tui]\nnarration_nudge_cap = 3\n").unwrap();
        assert_eq!(cfg.tui.unwrap().narration_nudge_cap, 3);
    }

    #[test]
    fn model_tuning_narration_nudge_cap_override_parses() {
        let cfg: Config = toml::from_str(
            r#"
            [[model_tuning]]
            model = "ornith:35b"
            narration_nudge_cap = 3
        "#,
        )
        .unwrap();
        let tune = cfg.find_model_tuning("ornith:35b").unwrap();
        assert_eq!(tune.narration_nudge_cap, Some(3));
        // Absent field stays None (inherit the [tui] value).
        let cfg: Config = toml::from_str(
            r#"
            [[model_tuning]]
            model = "other:7b"
            max_tool_rounds = 9
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.find_model_tuning("other:7b")
                .unwrap()
                .narration_nudge_cap,
            None
        );
    }

    #[test]
    fn tui_workflow_grace_rounds_can_be_overridden_or_disabled() {
        let cfg: Config = toml::from_str(
            r#"
            [tui]
            workflow_grace_rounds = 9
        "#,
        )
        .unwrap();
        assert_eq!(cfg.tui.unwrap().workflow_grace_rounds, 9);

        let disabled: Config = toml::from_str(
            r#"
            [tui]
            workflow_grace_rounds = 0
        "#,
        )
        .unwrap();
        assert_eq!(disabled.tui.unwrap().workflow_grace_rounds, 0);
    }

    #[test]
    fn model_tuning_parses_from_toml() {
        let toml = r#"
            [[model_tuning]]
            model = "nemotron3:33b"
            num_ctx = 24576
            mid_loop_trim_threshold = 12
            max_tool_rounds = 20
            workflow_grace_rounds = 8

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
        assert_eq!(nemo.workflow_grace_rounds, Some(8));

        let qwen = cfg.find_model_tuning("qwen3-coder:30b").unwrap();
        assert_eq!(qwen.num_ctx, Some(65536));
        assert_eq!(qwen.mid_loop_trim_threshold, None);
        assert_eq!(qwen.workflow_grace_rounds, None);
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
        assert_eq!(entry.workflow_grace_rounds, None);
    }

    // ---- #726: [tools] max_output_tokens ----

    #[test]
    fn tools_max_output_tokens_defaults_to_10k_when_absent() {
        // No `[tools]` section ⇒ the built-in default budget.
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.tools.is_none());
        assert_eq!(cfg.max_output_tokens(), 10_000);
        assert_eq!(cfg.output_head_tokens(), 1_500);
        assert_eq!(Config::default().max_output_tokens(), 10_000);
        assert_eq!(Config::default().output_head_tokens(), 1_500);
    }

    #[test]
    fn tools_output_cap_chars_per_token_defaults_to_3_and_parses_an_override() {
        // Absent ⇒ the conservative default (3, tighter than the 4 c/t estimate).
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.output_cap_chars_per_token(), 3);
        assert_eq!(Config::default().output_cap_chars_per_token(), 3);
        // A `[tools]` table that omits the key still falls back to 3.
        let cfg: Config = toml::from_str("[tools]\n").unwrap();
        assert_eq!(cfg.output_cap_chars_per_token(), 3);
        // Explicit override (e.g. 2 for very dense workloads) is honored.
        let cfg: Config = toml::from_str("[tools]\noutput_cap_chars_per_token = 2\n").unwrap();
        assert_eq!(cfg.tools.as_ref().unwrap().output_cap_chars_per_token, 2);
        assert_eq!(cfg.output_cap_chars_per_token(), 2);
    }

    #[test]
    fn tools_max_output_tokens_parses_an_override() {
        let cfg: Config = toml::from_str(
            r#"
            [tools]
            max_output_tokens = 4096
            output_head_tokens = 512
        "#,
        )
        .unwrap();
        assert_eq!(cfg.tools.as_ref().unwrap().max_output_tokens, 4096);
        assert_eq!(cfg.tools.as_ref().unwrap().output_head_tokens, 512);
        assert_eq!(cfg.max_output_tokens(), 4096);
        assert_eq!(cfg.output_head_tokens(), 512);
    }

    #[test]
    fn tools_config_default_field_is_the_shared_default() {
        // A `[tools]` table that omits the key falls back to the default fn.
        let cfg: Config = toml::from_str("[tools]\n").unwrap();
        assert_eq!(cfg.max_output_tokens(), 10_000);
        assert_eq!(cfg.output_head_tokens(), 1_500);
    }

    #[test]
    fn tools_max_output_tokens_zero_is_a_valid_no_cap() {
        let cfg: Config = toml::from_str("[tools]\nmax_output_tokens = 0\n").unwrap();
        assert_eq!(cfg.max_output_tokens(), 0);
    }

    #[test]
    fn tool_exposure_defaults_to_full_identity_when_absent() {
        // No `[tool_exposure]` section ⇒ the identity controller (unchanged
        // advertised catalog).
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.tool_exposure.is_none());
        let resolved = cfg.tool_exposure();
        assert_eq!(resolved.profile, ExposureProfile::Full);
        assert_eq!(resolved.schema_budget_pct, 15);
        assert_eq!(resolved.max_initial_tools, 0);
        assert!(resolved.supports_dynamic_catalog);
        assert_eq!(
            Config::default().tool_exposure().profile,
            ExposureProfile::Full
        );
    }

    #[test]
    fn tool_exposure_parses_an_auto_profile_override() {
        let cfg: Config = toml::from_str(
            r#"
            [tool_exposure]
            profile = "auto"
            schema_budget_pct = 12
            max_initial_tools = 8
            supports_dynamic_catalog = false
        "#,
        )
        .unwrap();
        let resolved = cfg.tool_exposure();
        assert_eq!(resolved.profile, ExposureProfile::Auto);
        assert_eq!(resolved.schema_budget_pct, 12);
        assert_eq!(resolved.max_initial_tools, 8);
        assert!(!resolved.supports_dynamic_catalog);
    }

    #[test]
    fn tool_exposure_minimal_profile_parses() {
        let cfg: Config = toml::from_str("[tool_exposure]\nprofile = \"minimal\"\n").unwrap();
        let resolved = cfg.tool_exposure();
        assert_eq!(resolved.profile, ExposureProfile::Minimal);
        // Omitted keys fall back to the shared defaults.
        assert_eq!(resolved.schema_budget_pct, 15);
    }
}

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
mod select_backend_tests {
    use super::*;

    fn openai(name: &str, api: OpenAiApi, endpoint: &str) -> BackendConfig {
        BackendConfig {
            name: name.into(),
            endpoint: endpoint.into(),
            model: Some("m".into()),
            tiers: vec![Tier::Fast],
            kind: Some(BackendKind::Openai),
            api: Some(api),
            ..Default::default()
        }
    }

    fn ollama(name: &str, endpoint: &str) -> BackendConfig {
        BackendConfig {
            name: name.into(),
            endpoint: endpoint.into(),
            model: Some("llama3.1:8b".into()),
            tiers: vec![Tier::Fast],
            kind: Some(BackendKind::Ollama),
            ..Default::default()
        }
    }

    fn plugin(name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.into(),
            command: "newt-provider-openai".into(),
            model: Some("gpt-test".into()),
            env_pass: vec![],
            tiers: vec![Tier::Complex],
        }
    }

    fn cfg(
        backends: Vec<BackendConfig>,
        providers: Vec<ProviderConfig>,
        default: Option<&str>,
    ) -> Config {
        Config {
            backends,
            providers,
            default_backend: default.map(str::to_string),
            ..Config::default()
        }
    }

    /// An owned, comparable summary of a [`SelectionOutcome`] so a test can drop
    /// the borrow on `Config` before asserting (keeps env-restore panic-safe).
    fn summary(c: &Config) -> String {
        match c.select_backend() {
            SelectionOutcome::Selected(SelectedBackend::Configured(b)) => {
                format!("configured:{}:{}", b.name, b.endpoint)
            }
            SelectionOutcome::Selected(SelectedBackend::Provider(p)) => {
                format!("provider:{}", p.name)
            }
            SelectionOutcome::UnknownNamed(n) => format!("unknown:{n}"),
            SelectionOutcome::Unset => "unset".to_string(),
        }
    }

    /// Run `f` with `$NEWT_PROVIDER=value`, restoring the prior value afterwards.
    /// The closure returns an OWNED value so no borrow escapes the restore.
    fn with_newt_provider<T>(value: &str, f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("NEWT_PROVIDER").ok();
        unsafe { std::env::set_var("NEWT_PROVIDER", value) };
        let out = f();
        match prev {
            Some(p) => unsafe { std::env::set_var("NEWT_PROVIDER", p) },
            None => unsafe { std::env::remove_var("NEWT_PROVIDER") },
        }
        out
    }

    /// Guarantee `$NEWT_PROVIDER` is unset for an env-free scenario (so the lane
    /// is deterministic regardless of a stray ambient value), restoring after.
    fn without_newt_provider<T>(f: impl FnOnce() -> T) -> T {
        let prev = std::env::var("NEWT_PROVIDER").ok();
        unsafe { std::env::remove_var("NEWT_PROVIDER") };
        let out = f();
        if let Some(p) = prev {
            unsafe { std::env::set_var("NEWT_PROVIDER", p) };
        }
        out
    }

    // 1. default_backend selects Ollama while OpenAI is ALSO configured.
    //    "mixed ⇒ OpenAI wins" is WRONG when Ollama was explicitly selected.
    #[test]
    #[serial_test::serial(newt_provider_env)]
    fn default_backend_selects_ollama_over_configured_openai() {
        let c = cfg(
            vec![
                ollama("local", "http://ollama:11434/"),
                openai("cloud", OpenAiApi::ChatCompletions, "http://vllm:8000/"),
            ],
            vec![],
            Some("local"),
        );
        assert_eq!(
            without_newt_provider(|| summary(&c)),
            "configured:local:http://ollama:11434/"
        );
    }

    // 2. $NEWT_PROVIDER selects Ollama (over an also-configured OpenAI backend).
    #[test]
    #[serial_test::serial(newt_provider_env)]
    fn newt_provider_selects_ollama() {
        let c = cfg(
            vec![
                ollama("local", "http://ollama:11434/"),
                openai("cloud", OpenAiApi::ChatCompletions, "http://vllm:8000/"),
            ],
            vec![],
            None,
        );
        assert_eq!(
            with_newt_provider("local", || summary(&c)),
            "configured:local:http://ollama:11434/"
        );
    }

    // 3. $NEWT_PROVIDER selects the OpenAI *Chat Completions* backend by name.
    #[test]
    #[serial_test::serial(newt_provider_env)]
    fn newt_provider_selects_openai_chat_completions() {
        let c = cfg(
            vec![
                openai(
                    "cloud-chat",
                    OpenAiApi::ChatCompletions,
                    "http://chat:8000/",
                ),
                openai("cloud-resp", OpenAiApi::Responses, "http://resp:8000/"),
            ],
            vec![],
            None,
        );
        assert_eq!(
            with_newt_provider("cloud-chat", || summary(&c)),
            "configured:cloud-chat:http://chat:8000/"
        );
    }

    // 4. $NEWT_PROVIDER selects the OpenAI *Responses* backend by name — the same
    //    config as (3), a different selector, a different destination.
    #[test]
    #[serial_test::serial(newt_provider_env)]
    fn newt_provider_selects_openai_responses() {
        let c = cfg(
            vec![
                openai(
                    "cloud-chat",
                    OpenAiApi::ChatCompletions,
                    "http://chat:8000/",
                ),
                openai("cloud-resp", OpenAiApi::Responses, "http://resp:8000/"),
            ],
            vec![],
            None,
        );
        assert_eq!(
            with_newt_provider("cloud-resp", || summary(&c)),
            "configured:cloud-resp:http://resp:8000/"
        );
    }

    // 5. A selected provider-plugin backend (named via default_backend), even
    //    with an OpenAI backend also present.
    #[test]
    #[serial_test::serial(newt_provider_env)]
    fn selects_provider_plugin_when_named() {
        let c = cfg(
            vec![openai(
                "cloud",
                OpenAiApi::ChatCompletions,
                "http://vllm:8000/",
            )],
            vec![plugin("myplugin")],
            Some("myplugin"),
        );
        assert_eq!(without_newt_provider(|| summary(&c)), "provider:myplugin");
    }

    // 6. An explicitly selected UNSUPPORTED backend still selects *that* entry —
    //    the "unsupported" verdict is the instantiator's job (worker suite), not
    //    a reason for the selector to pick a different backend.
    #[test]
    #[serial_test::serial(newt_provider_env)]
    fn explicitly_selected_backend_is_returned_even_if_unusual_kind() {
        let mut embedded = BackendConfig {
            name: "in-proc".into(),
            endpoint: "http://in-proc/".into(),
            kind: Some(BackendKind::Embedded),
            model: Some("tiny".into()),
            ..Default::default()
        };
        embedded.tiers = vec![Tier::Fast];
        let c = cfg(
            vec![
                embedded,
                openai("cloud", OpenAiApi::ChatCompletions, "http://vllm:8000/"),
            ],
            vec![],
            Some("in-proc"),
        );
        // The Embedded backend is what was selected — NOT the OpenAI one.
        assert_eq!(
            without_newt_provider(|| summary(&c)),
            "configured:in-proc:http://in-proc/"
        );
    }

    // 7. No configured backend ⇒ Unset, which alone permits local discovery.
    #[test]
    #[serial_test::serial(newt_provider_env)]
    fn no_configured_backend_is_unset() {
        let c = cfg(vec![], vec![], None);
        assert_eq!(without_newt_provider(|| summary(&c)), "unset");
    }

    // 8. An explicit selector naming a nonexistent entry is UnknownNamed — an
    //    operator error, NOT a silent fallback to the present OpenAI backend.
    #[test]
    #[serial_test::serial(newt_provider_env)]
    fn unknown_named_backend_is_an_error_not_a_fallback() {
        let c = cfg(
            vec![openai(
                "cloud",
                OpenAiApi::ChatCompletions,
                "http://vllm:8000/",
            )],
            vec![],
            Some("ghost"),
        );
        assert_eq!(without_newt_provider(|| summary(&c)), "unknown:ghost");
        // And the same via $NEWT_PROVIDER (the live override), which must not
        // silently defer to default_backend or to preference.
        let c2 = cfg(
            vec![openai(
                "cloud",
                OpenAiApi::ChatCompletions,
                "http://vllm:8000/",
            )],
            vec![],
            None,
        );
        assert_eq!(with_newt_provider("typo", || summary(&c2)), "unknown:typo");
    }

    // Guard: preference still prefers OpenAI when NOTHING is explicitly selected
    // (the historical default is preserved — only explicit selection overrides it).
    #[test]
    #[serial_test::serial(newt_provider_env)]
    fn prefers_openai_when_nothing_is_explicitly_selected() {
        let c = cfg(
            vec![
                ollama("local", "http://ollama:11434/"),
                openai("cloud", OpenAiApi::ChatCompletions, "http://vllm:8000/"),
            ],
            vec![],
            None,
        );
        assert_eq!(
            without_newt_provider(|| summary(&c)),
            "configured:cloud:http://vllm:8000/"
        );
    }
}
