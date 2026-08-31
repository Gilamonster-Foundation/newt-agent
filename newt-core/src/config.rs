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
// kebab-case, NOT lowercase: serde's `lowercase` rule is a plain
// `to_ascii_lowercase()` of the identifier, which would name `AppendOnly`
// "appendonly" while `keyword()`, the `/context manager` selector and the docs
// all say "append-only" — making the documented spelling a hard config-load
// failure. `standard` / `progressive` / `distributed` are byte-identical under
// either rule, so this is not a compatibility break for existing configs.
#[serde(rename_all = "kebab-case")]
pub enum ContextManager {
    /// Prune → summarize → static-marker (today's behavior). The only one
    /// implemented; the selector seam for the others.
    #[default]
    Standard,
    /// **Never rewrite history.** No summarization, no structural pruning of
    /// prior messages: this preset removes compaction as a source of
    /// prompt-prefix churn, so whatever prefix the harness emits stays stable
    /// turn over turn and provider caching can hold. Oversized material is
    /// capped where it is *produced* (tool-result caps, paginated reads,
    /// offload). A request that will not fit an **authoritative** ceiling is
    /// refused rather than rewritten; softer triggers dispatch as-is, since
    /// dispatching rewrites nothing either.
    ///
    /// The preset governs compaction, not every byte of the request: a caller
    /// that regenerates a volatile system prompt each turn still invalidates the
    /// cache on its own, and that is the caller's to fix — not something this
    /// preset can promise away.
    ///
    /// This is the strategy `mini-swe-agent` uses, and it is a real answer, not
    /// a degenerate one: it scores >74% on SWE-bench Verified with no context
    /// management at all. It trades recall for fidelity — nothing is silently
    /// altered, because nothing is altered.
    AppendOnly,
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
            "append-only" | "append_only" | "appendonly" | "append" => Some(Self::AppendOnly),
            "progressive" => Some(Self::Progressive),
            "distributed" => Some(Self::Distributed),
            _ => None,
        }
    }

    /// The canonical keyword. Round-trips through BOTH `from_keyword` and serde
    /// — the `kebab-case` rename on the enum is what keeps the second half of
    /// that promise once a variant's name is more than one word.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::AppendOnly => "append-only",
            Self::Progressive => "progressive",
            Self::Distributed => "distributed",
        }
    }

    /// Whether this manager is implemented. Only `standard` today; the others
    /// are owned by #546 (the selector reports "not yet available").
    pub fn available(self) -> bool {
        matches!(self, Self::Standard | Self::AppendOnly)
    }

    /// Whether this preset may rewrite messages already in the transcript.
    ///
    /// `false` for [`AppendOnly`](Self::AppendOnly), which is the whole point of
    /// it: no summarization, no structural pruning of prior turns, so the prompt
    /// prefix is byte-identical turn over turn and provider caching is optimal by
    /// construction. Everything else rewrites, and pays the cache cost for recall.
    pub fn rewrites_history(self) -> bool {
        !matches!(self, Self::AppendOnly)
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
    pub fn bundle_profile_for_family<'a>(
        &self,
        bundle: &'a BundleConfig,
        family: Option<&str>,
    ) -> Option<&'a str> {
        family
            .and_then(|fam| {
                bundle
                    .families
                    .iter()
                    .find(|(key, _)| key.as_str() == fam)
                    .map(|(_, p)| p.as_str())
            })
            .or(bundle.default_profile.as_deref())
    }

    /// Infer the bundle for the TYPED model family (the resolved card's
    /// declared metadata under the route-association gates — never a
    /// model-name prefix): a bundle applies when its `applies_to` names the
    /// family EXACTLY. No family ⇒ no automatic bundle — a qwen-LOOKING
    /// alias with no exact card gets no family behavior (the anti-substring
    /// law: names are labels, never evidence). Only bundles with a
    /// non-empty `applies_to` participate — a use-case bundle (empty
    /// `applies_to`) is never auto-inferred, only chosen explicitly via
    /// `--bundle`.
    #[must_use]
    pub fn infer_bundle_for_family(&self, family: Option<&str>) -> Option<(&str, &BundleConfig)> {
        let fam = family?;
        self.bundles
            .iter()
            .find(|(_, b)| b.applies_to.iter().any(|a| a == fam))
            .map(|(name, b)| (name.as_str(), b))
    }

    /// Resolve the active profile from the selectors + the TYPED model
    /// family: `--profile` (explicit) > `--bundle` (its profile for this
    /// family) > a bundle inferred from the exact family (`applies_to`) >
    /// `None`. Automatic selection keys on the resolved card's declared
    /// family under the route-association gates — NEVER on model-name
    /// prefixes. Returns the profile NAME + how it was chosen (for the
    /// banner).
    ///
    /// # Errors
    /// An unknown explicit `--bundle` is a hard error. An unknown explicit
    /// `--profile` is left for the caller's [`resolve_profile`](Self::resolve_profile)
    /// so the message stays profile-specific.
    pub fn pick_active_profile(
        &self,
        profile_flag: Option<&str>,
        bundle_flag: Option<&str>,
        family: Option<&str>,
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
                .bundle_profile_for_family(bundle, family)
                .map(|p| ProfilePick {
                    name: p.to_string(),
                    via: PickVia::Bundle(b.to_string()),
                }));
        }
        if let Some((name, bundle)) = self.infer_bundle_for_family(family) {
            return Ok(self
                .bundle_profile_for_family(bundle, family)
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
/// applies_to = ["nemotron"]                 # EXACT typed family names (card metadata)
/// default_profile = "nemotron"
/// families = { "nemotron" = "nemotron", "qwen3" = "qwen-coder" }
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

/// Explicit ownership of a backend drop-in record — who may rewrite the file
/// and how the loader merges it. This is the discriminator the loader
/// BRANCHES on; [`BackendProvenance`] below stays purely informational.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RecordTag {
    /// Operator-owned: the loader replaces the same-name backend WHOLESALE,
    /// so omissions deliberately clear/rebind. The runtime writeback never
    /// touches such a file. Untagged files are operator-owned too — except
    /// the legacy ambiguity the backend assembly's drop-in merge refuses to guess
    /// about (see there).
    OperatorV1,
    /// Probe-owned overlay: associated by exact name + endpoint, whitelist-
    /// merged (observed `kind`/`api`/`serving`, plus `model` only for an
    /// Instance observation) onto the same-name backend. Never touches card,
    /// capability, auth, tiers, managed, host, or operator provenance.
    ProbeV1,
}

/// Where a backend file came from — written by `newt setup`, hand-authored,
/// or probe-derived. Pure data; nothing branches on it (ownership branches
/// on [`RecordTag`]). Makes a generated file self-describing and lets
/// `doctor` show declared-vs-derived drift.
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
    /// Overlay `edits` onto a per-file backend drop-in's TOML `text`,
    /// **preserving comments, key order, and every key newt does not model** —
    /// unlike a serde round-trip (`toml::from_str` → mutate → `toml::to_string`),
    /// which silently destroys both. Pure: the caller owns the read/write, the
    /// same contract as [`Config::with_default_backend`], which exists for
    /// exactly this reason on the config side.
    ///
    /// Each edit is `(key, value)`: `Some` sets that top-level key to the string
    /// (creating it when absent, keeping the existing line's decor when
    /// present), `None` removes it. Only string scalars are settable, which
    /// covers every field the backend panel's form manages (`kind`, `endpoint`,
    /// `model`, `api_key_env`, `api_key_file`, `name`); an edit list that omits
    /// a key leaves it byte-for-byte alone.
    ///
    /// # Errors
    /// Returns [`NewtError::Config`] when `text` is not valid TOML.
    pub fn with_dropin_edits(text: &str, edits: &[(&str, Option<String>)]) -> Result<String> {
        let mut doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|e| NewtError::Config(format!("backend drop-in is not valid TOML: {e}")))?;
        let root = doc.as_table_mut();
        for (key, value) in edits {
            match value {
                Some(new) => match root.get_mut(key) {
                    Some(item) => {
                        // Keep the operator's trailing comment / spacing on a
                        // key that already exists.
                        let decor = item.as_value().map(|value| value.decor().clone());
                        *item = toml_edit::value(new.as_str());
                        if let (Some(decor), Some(value)) = (decor, item.as_value_mut()) {
                            *value.decor_mut() = decor;
                        }
                    }
                    None => {
                        root.insert(key, toml_edit::value(new.as_str()));
                    }
                },
                None => {
                    root.remove(key);
                }
            }
        }
        Ok(doc.to_string())
    }

    /// Resolve explicitly accepted Chat Completions request extensions —
    /// from the INLINE block only. The card-aware answer is
    /// [`crate::model_card::ResolvedCapabilities`], constructed once per
    /// backend choice; these inline accessors stay for the card-less callers
    /// and as the conservative floor.
    #[must_use]
    pub fn chat_completions_capability(&self) -> crate::model_card::ChatCompletionsCapability {
        self.capability
            .as_ref()
            .and_then(|capability| capability.chat_completions)
            .unwrap_or_default()
    }

    /// Whether this model streams its chain-of-thought as a lone leading
    /// closer (`reasoning</think>answer`, no opening tag) and therefore needs
    /// the stream filter to start INSIDE the reasoning block.
    ///
    /// Reads THIS backend's inline [`Capability`] — never the model name.
    /// Display names are labels: an operator may serve any artifact under any
    /// alias, so `contains("qwen3")` is wrong in both directions (it
    /// suppresses output from things that are not Qwen, and prints raw
    /// reasoning from things that are). Replaces the #384 name-list stopgap.
    ///
    /// **Scope:** like its two siblings above, this reads the inline
    /// `capability` field only — the conservative floor. The card-aware
    /// surface is [`crate::model_card::ResolvedCapabilities`], which resolves
    /// the named `card =` binding once per backend choice and decides per
    /// serving principal; every runtime lane consumes that, not this.
    ///
    /// **Unknown defaults to `false` — do not suppress.** The two failure
    /// modes are not symmetric: filtering when we should not DROPS real answer
    /// text silently, while not filtering when we should shows reasoning the
    /// operator can see and correct. Fail toward the visible one.
    #[must_use]
    pub fn emits_leading_reasoning(&self) -> bool {
        self.capability
            .as_ref()
            .and_then(|capability| capability.emits_leading_reasoning)
            .unwrap_or(false)
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

/// A side call's backend, expressed as an **override of the session backend**.
///
/// Every field is optional and an absent field inherits the session's value, so
/// an empty table means "run this exactly where the session runs". This is the
/// same inherit-or-override shape `[summarizer]` already uses; it is factored
/// out here so a second consumer does not hand-roll a third spelling of
/// *(endpoint, model, kind, key)*.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackendRef {
    /// `None` ⇒ reuse the session endpoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// `None` ⇒ reuse the session model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `None` ⇒ reuse the session wire protocol.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<BackendKind>,
    /// Bearer-token environment variable (checked before `api_key_file`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Bearer-token file (first non-empty line).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_file: Option<String>,
}

impl BackendRef {
    /// Does this override point somewhere other than the session endpoint?
    #[must_use]
    pub fn pins_a_different_host(&self) -> bool {
        self.endpoint.is_some()
    }

    /// Resolve against the session's `(endpoint, model, kind, key)`.
    ///
    /// The api-key rule is the one that matters and mirrors the summarizer's
    /// (`resolve_summarizer_backend`): **a bearer token authenticates a
    /// specific host**, so the session key is inherited only when this call
    /// reuses the session endpoint. Pinning a different host and inheriting the
    /// session's credential would leak it.
    #[must_use]
    pub fn resolve(
        &self,
        session_endpoint: &str,
        session_model: &str,
        session_kind: BackendKind,
        session_key: &Option<String>,
    ) -> (String, String, BackendKind, Option<String>) {
        let own_key =
            resolve_api_key_common(self.api_key_env.as_deref(), self.api_key_file.as_deref())
                .unwrap_or_default();
        let key = if self.pins_a_different_host() {
            own_key
        } else {
            own_key.or_else(|| session_key.clone())
        };
        (
            self.endpoint
                .clone()
                .unwrap_or_else(|| session_endpoint.to_string()),
            self.model
                .clone()
                .unwrap_or_else(|| session_model.to_string()),
            self.kind.unwrap_or(session_kind),
            key,
        )
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

/// A backend's EXACT destination — where the session's bytes go: an HTTP
/// `endpoint`, or (`kind = "embedded"`) a local `model_path`. The ONLY
/// normalization anywhere is empty-string-to-`None`; comparison is exact
/// string equality, never URL parsing or trimming (a near-collision must
/// compare unequal, not get "helpfully" unified).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendDestination {
    /// The HTTP endpoint, when one is declared/requested (empty ⇒ `None`).
    pub endpoint: Option<String>,
    /// The local artifact path for an embedded backend.
    pub model_path: Option<String>,
}

impl BackendDestination {
    /// Empty-to-`None` construction — the one normalization.
    #[must_use]
    pub fn new(endpoint: Option<String>, model_path: Option<String>) -> Self {
        Self {
            endpoint: endpoint.filter(|e| !e.is_empty()),
            model_path: model_path.filter(|p| !p.is_empty()),
        }
    }

    /// The destination a backend declaration names.
    #[must_use]
    pub fn of(backend: &BackendConfig) -> Self {
        Self::new(Some(backend.endpoint.clone()), backend.model_path.clone())
    }

    /// A CONCRETE destination: exactly one NONEMPTY axis (endpoint XOR
    /// model_path). A hollow destination (neither) routes nowhere and a
    /// composite one (both) is two identities — neither may anchor an exact
    /// association ([`crate::model_card::ResolvedCapabilities::for_route`]
    /// refuses to activate a card across a non-concrete destination). The
    /// fields are public, so a hand-built literal can hold `Some("")` that
    /// [`BackendDestination::new`] would have normalized away — concreteness
    /// therefore checks CONTENT, not `Option` presence: two empty-string
    /// endpoints agreeing are two absences, not an identity.
    #[must_use]
    pub fn is_concrete(&self) -> bool {
        let endpoint = self.endpoint.as_deref().is_some_and(|e| !e.is_empty());
        let model_path = self.model_path.as_deref().is_some_and(|p| !p.is_empty());
        endpoint ^ model_path
    }
}

/// The operator's DECLARED backend facts — the layer as configured (inline
/// `[[backends]]` or an `operator_v1` drop-in), before any probe overlay or
/// CLI request. Immutable intent: never probe residue, never a request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeclaredBackend {
    /// Where the operator pointed this backend.
    pub destination: BackendDestination,
    /// The declared model, if any.
    pub model: Option<String>,
    /// The declared model card, if any.
    pub card: Option<String>,
    /// The declared serving axis.
    pub serving: Option<Serving>,
    /// The declared wire protocol.
    pub kind: Option<BackendKind>,
    /// The declared OpenAI HTTP surface.
    pub api: Option<OpenAiApi>,
    /// The declared managed mode.
    pub managed: Option<ManagedMode>,
}

impl DeclaredBackend {
    /// Snapshot the declaration layer from a backend that IS pure
    /// declaration (nothing has overlaid it).
    #[must_use]
    pub fn of(backend: &BackendConfig) -> Self {
        Self {
            destination: BackendDestination::of(backend),
            // The effective-model rule: an empty/whitespace model string is
            // NO model identity — it must never become an exact identifier
            // a card binding could associate against.
            model: backend.effective_model().map(str::to_string),
            card: backend.card.clone(),
            serving: backend.serving,
            kind: backend.kind,
            api: backend.api,
            managed: backend.managed,
        }
    }
}

/// How a CLI `--backend-*` request targets the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestMode {
    /// A destination (`--backend-url` / `--backend-model-path`) was given:
    /// the request defines an EXCLUSIVE backend — one slot survives.
    ExclusiveDestination,
    /// Field-only: the named (else first) backend is edited in place.
    FieldOnly,
}

/// The explicit per-invocation CLI request, recorded AS a request — typed
/// facts taken from the `--backend-*` flags themselves, never re-derived
/// from the mutated backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendRequest {
    /// Exclusive-destination or field-only (see [`RequestMode`]).
    pub mode: RequestMode,
    /// The requested endpoint, if any (empty ⇒ `None`).
    pub endpoint: Option<String>,
    /// The requested embedded artifact path, if any.
    pub model_path: Option<String>,
    /// The requested model, if any.
    pub model: Option<String>,
    /// The requested card rebind, if any.
    pub card: Option<String>,
    /// The requested serving axis, if any.
    pub serving: Option<Serving>,
    /// The requested wire protocol, if any.
    pub kind: Option<BackendKind>,
    /// The requested OpenAI HTTP surface, if any.
    pub api: Option<OpenAiApi>,
}

impl BackendRequest {
    fn from_override(over: &BackendOverride) -> Self {
        let mode = if over.endpoint.is_some() || over.model_path.is_some() {
            RequestMode::ExclusiveDestination
        } else {
            RequestMode::FieldOnly
        };
        Self {
            mode,
            endpoint: over.endpoint.clone().filter(|e| !e.is_empty()),
            model_path: over.model_path.clone().filter(|p| !p.is_empty()),
            // Same effective-model rule as the declaration layer: an
            // empty/whitespace request is no model identity.
            model: over.model.clone().filter(|m| !m.trim().is_empty()),
            card: over.card.clone(),
            serving: over.serving,
            kind: over.kind,
            api: over.api,
        }
    }

    /// The destination the request lands on, given the declared one: the
    /// requested endpoint/model_path override their declared counterparts
    /// field-by-field; a request with neither lands where declared.
    #[must_use]
    pub fn destination_over(&self, declared: &BackendDestination) -> BackendDestination {
        if self.endpoint.is_none() && self.model_path.is_none() {
            return declared.clone();
        }
        // A destination request REPLACES the destination: `--backend-url`
        // points the exclusive backend at that URL (it does not inherit a
        // declared model_path, nor vice versa).
        BackendDestination {
            endpoint: self.endpoint.clone(),
            model_path: self.model_path.clone(),
        }
    }
}

/// Per-backend provenance receipt: the LAYERS a resolved backend was
/// composed from, kept distinguishable. [`Config::resolve`] flattens
/// operator declaration → cached `probe_v1` observation → per-invocation
/// CLI request into one effective [`BackendConfig`] — right for wire
/// routing, wrong for evidence: a consumer reading `backend.model` cannot
/// tell a declaration from probe residue or a request. Receipts are built
/// by the private backend assembly and ride in [`ResolvedConfig`], aligned
/// 1:1 by slot with `config.backends` — never looked up by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendResolutionReceipt {
    /// The operator's declaration layer.
    pub declaration: DeclaredBackend,
    /// The explicit CLI request, if any.
    pub request: Option<BackendRequest>,
    /// The cached probe observation this resolution retained, if any. A
    /// requested destination CHANGE clears it (cached truth about one
    /// server must not ride to another); an identical destination retains.
    pub observation: Option<ProbeObservation>,
    /// The card-binding evidence this resolution justifies — see
    /// [`crate::model_card::CardBindingSeed`]:
    ///
    /// * an explicit `--backend-card` is a deliberate rebind — the requested
    ///   card binds at the post-request destination, to the requested model
    ///   (else the declared one, NEVER a probed one);
    /// * otherwise the declared card binds to the declared model at the
    ///   declared destination — a model-only or endpoint-only request
    ///   RETAINS this binding untouched, and visibility is decided
    ///   downstream by typed applicability
    ///   ([`crate::model_card::ResolvedCapabilities::for_route`]), never by
    ///   erasing evidence here.
    pub binding: crate::model_card::CardBindingSeed,
}

/// Shared backend-identity validation: nonempty and unique names. Selection
/// (`default_backend`, `$NEWT_PROVIDER`), CLI overrides, drop-in merging,
/// and the slot-aligned receipts are all name-addressed at their edges —
/// with a duplicate, different consumers can disagree about WHICH backend a
/// name means and hand backend A the card binding declared for backend B.
/// Hard, actionable error instead. Used by the assembly constructor on
/// every path (normal resolve AND profiles) and again after the CLI
/// request.
fn validate_backend_names<'a>(
    backends: impl Iterator<Item = &'a BackendConfig>,
) -> std::result::Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for (i, b) in backends.enumerate() {
        if b.name.trim().is_empty() {
            return Err(format!(
                "backend #{} has no name — every [[backends]] entry needs a unique \
                 `name` (selection, overrides, and card bindings are name-based)",
                i + 1
            ));
        }
        if !seen.insert(b.name.clone()) {
            return Err(format!(
                "two backends share the name `{}` — backend selection is name-based \
                 everywhere (default_backend, $NEWT_PROVIDER, --backend-*, card \
                 bindings), so a duplicate can activate the wrong card; rename one",
                b.name
            ));
        }
    }
    Ok(())
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

/// A declaration with SOME destination — a nonempty endpoint or a nonempty
/// `model_path`. `model_path = ""` is NOT a destination: an empty-path
/// drop-in must not pass destination checks and strip a valid slot.
fn backend_has_destination(b: &BackendConfig) -> bool {
    !b.endpoint.is_empty() || b.model_path.as_deref().is_some_and(|p| !p.is_empty())
}

/// Shared destination-XOR validation for DECLARATIONS: a backend has ONE
/// destination — an HTTP `endpoint`, or an embedded `model_path`, never
/// both (a composite destination is two identities in one slot; every
/// consumer — routing, probe association, card bindings — would pick a
/// side silently). CLI requests get the same rule in
/// [`BackendAssembly::apply_request`].
fn validate_backend_destination(b: &BackendConfig) -> std::result::Result<(), String> {
    if !b.endpoint.is_empty() && b.model_path.as_deref().is_some_and(|p| !p.is_empty()) {
        return Err(format!(
            "backend `{}` declares BOTH an endpoint and a model_path — a backend has \
             ONE destination; remove one",
            b.name
        ));
    }
    Ok(())
}

/// A defensive name-lookup outcome — assembly operations never assume a
/// name resolves, even though the constructor validated uniqueness.
enum NameMatch {
    Missing,
    Unique(usize),
    Ambiguous,
}

/// One backend under assembly: the operator's declaration plus the layers
/// that may (or may not) apply to it. The layers stay SEPARATE until
/// [`BackendAssembly::finish`] composes the effective backend and mints the
/// receipt — so a later layer can never masquerade as an earlier one.
#[derive(Debug)]
struct AssemblySlot {
    /// The declaration: inline `[[backends]]`, replaced wholesale by an
    /// `operator_v1` drop-in.
    declaration: BackendConfig,
    /// The exact probe observation attached to this slot, if any.
    observation: Option<ProbeObservation>,
    /// The CLI `--backend-*` request targeted at this slot, if any.
    request: Option<BackendOverride>,
}

impl AssemblySlot {
    fn declared(declaration: BackendConfig) -> Self {
        Self {
            declaration,
            observation: None,
            request: None,
        }
    }

    /// The declaration with this slot's observation overlaid — the
    /// PROBE-INFORMED effective view (pre-request). Used both by
    /// [`BackendAssembly::finish`]'s composition and by the CLI targeting
    /// in [`BackendAssembly::apply_request`], so the backend a field-only
    /// edit lands on and the backend the final resolution selects can
    /// never diverge over probed facts.
    fn observed_view(&self) -> BackendConfig {
        let mut backend = self.declaration.clone();
        if let Some(obs) = &self.observation {
            overlay_observation(&mut backend, obs);
        }
        normalize_destination_kind(&mut backend);
        backend
    }
}

/// Destination/kind coherence normalization — the SAME rule in composition
/// ([`BackendAssembly::finish`]) and the targeting preview
/// ([`AssemblySlot::observed_view`]), so a declaration the composition
/// would accept (model_path + a stale HTTP kind, normalized to Embedded)
/// is never refused by a harmless field-only edit that previewed it
/// un-normalized. Both axes:
///
/// * **kind** — a model_path route IS embedded; an endpoint route never
///   retains Embedded (cleared to probe-at-connect);
/// * **serving** — an embedded backend serves exactly ONE artifact
///   ([`derive_serving`] makes Embedded intrinsically Instance), so a
///   model_path route never retains an inherited/declared Multiplexer —
///   Phase B's principal decision must never see
///   `kind = Embedded + serving = multiplexer`. (EXPLICITLY contradictory
///   requests are rejected in `apply_request`, not normalized away.)
fn normalize_destination_kind(backend: &mut BackendConfig) {
    if backend.endpoint.is_empty() && backend.model_path.as_deref().is_some_and(|p| !p.is_empty()) {
        backend.kind = Some(BackendKind::Embedded);
        backend.serving = Some(Serving::Instance);
    } else if !backend.endpoint.is_empty() && backend.kind == Some(BackendKind::Embedded) {
        backend.kind = None;
    }
}

/// Overlay a probe observation's facts onto a backend — only what a probe
/// observes: `kind`/`api`/`serving`, plus the model iff Instance (the typed
/// [`ProbeObservation::serving_axis`] gate). The ONE overlay, shared by
/// composition and targeting.
fn overlay_observation(backend: &mut BackendConfig, obs: &ProbeObservation) {
    if let Some(kind) = obs.kind {
        backend.kind = Some(kind);
    }
    if let Some(api) = obs.api {
        backend.api = Some(api);
    }
    let (serving, model) = obs.serving_axis();
    if let Some(serving) = serving {
        backend.serving = Some(serving);
        // Only an Instance observation carries backend-truth model; a
        // multiplexer/unknown observation leaves the declared model
        // standing.
        if let Some(model) = model {
            backend.model = Some(model);
        }
    }
}

/// A probe record staged during the directory walk, attached only after
/// EVERY directory's operator declarations have applied — so a probe in an
/// earlier directory is judged against the FINAL declaration, not against
/// whichever declaration happened to exist when its file was read.
#[derive(Debug)]
struct PendingProbe {
    path: PathBuf,
    stem: String,
    observation: ProbeObservation,
}

/// The PRIVATE backend assembly: the one place the four layers of a
/// backend meet, in order — inline/project declaration → operator drop-in
/// replacement → exact probe observation → CLI request. Owns the layering
/// rules so [`ResolvedConfig`]'s receipts are correct BY CONSTRUCTION:
///
/// * the constructor validates backend identity (nonempty, unique names)
///   on every path — normal resolve and profiles alike;
/// * an operator drop-in REPLACES its slot's declaration and resets the
///   slot's observation (the file IS the backend);
/// * a probe record attaches only to the UNIQUE slot with the exact same
///   name AND destination — cached truth about one server never rides to
///   another;
/// * the CLI request is recorded as a request; an exclusive destination
///   request retains exactly one (chosen or new) slot.
#[derive(Debug)]
struct BackendAssembly {
    slots: Vec<AssemblySlot>,
    /// Probe records staged for post-declaration attachment, in walk order
    /// (directory precedence, then path order) — attachment is last-wins,
    /// so a later directory's probe record deterministically supersedes an
    /// earlier one for the same slot.
    pending_probes: Vec<PendingProbe>,
    /// An operator drop-in merged — the config is operator-configured.
    operator_configured: bool,
    /// A nonempty CLI request was applied.
    requested: bool,
    /// #1984: every skip/degrade decision this assembly made, as VALUES —
    /// the primary record. `warn` (below) is the ONE place that both
    /// appends here and emits the `tracing::warn!` a human `RUST_LOG=warn`
    /// session still sees; every other call site in this impl block goes
    /// through it rather than calling `tracing::warn!` directly, so there
    /// is exactly one emission point to keep in sync. Tests assert on
    /// `warnings()`, not on a scraped log — see `config_tests/tests.rs`'s
    /// module doc for why the log-scraping shape was flaky (a per-test
    /// `tracing::subscriber::with_default` capture races tracing's
    /// process-wide callsite interest cache against sibling tests doing
    /// the same, #1984).
    warnings: Vec<String>,
}

impl BackendAssembly {
    /// Stage `backends` (pure declarations) for assembly, validating
    /// backend identity first — see [`validate_backend_names`].
    fn new(backends: Vec<BackendConfig>) -> std::result::Result<Self, String> {
        validate_backend_names(backends.iter())?;
        for b in &backends {
            validate_backend_destination(b)?;
        }
        Ok(Self {
            slots: backends.into_iter().map(AssemblySlot::declared).collect(),
            pending_probes: Vec::new(),
            operator_configured: false,
            requested: false,
            warnings: Vec::new(),
        })
    }

    /// The ONE place this impl block records a skip/degrade decision:
    /// appends `message` to the returned-value record (#1984's fix) and
    /// emits it as a `tracing::warn!` so an operator with `RUST_LOG=warn`
    /// (the default — see #1951) still sees it live. `message` should read
    /// the same whether it reaches a human via the log or a test via
    /// [`Self::warnings`].
    fn warn(&mut self, message: String) {
        tracing::warn!("{message}");
        self.warnings.push(message);
    }

    /// Every skip/degrade decision recorded so far, in the order they
    /// happened — the returned-value record `warn` builds. Callable any
    /// time before [`Self::finish`] consumes `self`.
    ///
    /// `#[cfg(test)]`: `warn` (above) is the sole PRODUCTION consumer of
    /// `self.warnings` today (it feeds `tracing::warn!`) — nothing in
    /// production reads the accumulated Vec back out yet. `newt doctor`'s
    /// drop-in diagnostics (#1951/#1962) were checked as a candidate
    /// consumer and are NOT: that scan deliberately does not call
    /// `merge_dir` at all, because it must keep reporting file-by-file even
    /// when `merge_dir` hard-errors (the ambiguous-legacy-marker case) —
    /// exactly the failure this accessor's caller would already be past.
    /// Un-gate this the day a production caller needs it; until then,
    /// `#[cfg(test)]` is the honest signal that it is a value the TESTS
    /// rely on, not a currently-dead production API surface.
    #[cfg(test)]
    fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn operator_configured(&self) -> bool {
        self.operator_configured
    }

    fn requested(&self) -> bool {
        self.requested
    }

    /// The compiled-in localhost fallback, staged as a declaration.
    fn push_fallback(&mut self, backend: BackendConfig) {
        self.slots.push(AssemblySlot::declared(backend));
    }

    fn find(&self, name: &str) -> NameMatch {
        let mut hits = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.declaration.name == name)
            .map(|(i, _)| i);
        match (hits.next(), hits.next()) {
            (None, _) => NameMatch::Missing,
            (Some(i), None) => NameMatch::Unique(i),
            (Some(_), Some(_)) => NameMatch::Ambiguous,
        }
    }

    /// Merge `<dir>/*.toml` drop-ins (filename stem = name), branching on
    /// the file's raw `record` header:
    ///
    /// * **Operator records** (`record = "operator_v1"`, or untagged and
    ///   classified operator) — REPLACE the same-name slot's declaration
    ///   wholesale (resetting its observation), else append a new slot.
    ///   Omissions deliberately clear/rebind; the file IS the backend.
    /// * **Probe records** (`record = "probe_v1"`, or an unambiguous
    ///   legacy probe cache) — parsed through the STRICT machine schema
    ///   and STAGED; they attach as slot observations only after every
    ///   directory's declarations have applied (see
    ///   [`Self::attach_pending_probes`]), so a home-dir probe survives to
    ///   be judged against a project-dir declaration. Never card,
    ///   capability, auth, tiers, managed, host, or operator provenance;
    ///   an invalid record is skipped with a visible warning.
    ///
    /// A malformed file is skipped with a warning. The one HARD ERROR is
    /// the legacy ambiguity: a file carrying the exact old newt-adopt probe
    /// marker AND binding/operator evidence cannot be attributed (operator
    /// declaration, or probe residue?) — refuse to guess, name the path and
    /// the remediations.
    fn merge_dir(&mut self, dir: &Path) -> std::result::Result<(), String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(()); // no backends dir — fine
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
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let tag = match disk_record_tag(&text) {
                Ok(tag) => tag,
                Err(e) => {
                    self.warn(format!(
                        "{}: skipping malformed backend file: {e}",
                        path.display()
                    ));
                    continue;
                }
            };
            match tag {
                Some(RecordTag::ProbeV1) => self.stage_probe(&path, stem, &text),
                Some(RecordTag::OperatorV1) => self.merge_operator(&path, stem, &text),
                None => {
                    let backend = match toml::from_str::<BackendConfig>(&text) {
                        Ok(backend) => backend,
                        Err(e) => {
                            // The header parse is laxer than the full parse
                            // (it reads one key) — a body-malformed file
                            // lands here, same visible skip as everywhere.
                            self.warn(format!(
                                "{}: skipping malformed backend file: {e}",
                                path.display()
                            ));
                            continue;
                        }
                    };
                    match classify_untagged_dropin(&backend, &text) {
                        Ok(DropinOwner::Operator) => self.merge_operator(&path, stem, &text),
                        Ok(DropinOwner::Probe) => self.stage_probe(&path, stem, &text),
                        Err(reason) => return Err(format!("{}: {reason}", path.display())),
                    }
                }
            }
        }
        Ok(())
    }

    /// An operator record: the file IS the backend — its declaration
    /// replaces the slot wholesale and resets the slot's observation. It
    /// needs a destination — an HTTP `endpoint`, or (`kind = "embedded"`)
    /// a local `model_path`; a record with neither is skipped TOUCHING
    /// NOTHING (a skipped record must not strip what an earlier layer
    /// established).
    fn merge_operator(&mut self, path: &Path, stem: &str, text: &str) {
        let mut backend = match toml::from_str::<BackendConfig>(text) {
            Ok(backend) => backend,
            Err(e) => {
                self.warn(format!(
                    "{}: skipping malformed backend file: {e}",
                    path.display()
                ));
                return;
            }
        };
        // The filename is authoritative for the name (collision-free).
        backend.name = stem.to_string();
        if !backend_has_destination(&backend) {
            self.warn(format!(
                "{}: skipping backend with neither endpoint nor model_path",
                path.display()
            ));
            return;
        }
        if let Err(reason) = validate_backend_destination(&backend) {
            self.warn(format!(
                "{}: skipping backend drop-in: {reason}",
                path.display()
            ));
            return;
        }
        self.operator_configured = true;
        match self.find(stem) {
            NameMatch::Unique(i) => self.slots[i] = AssemblySlot::declared(backend),
            NameMatch::Missing => self.slots.push(AssemblySlot::declared(backend)),
            NameMatch::Ambiguous => {
                // Unreachable (the constructor validated uniqueness) — but
                // never guess which duplicate a file means.
                self.warn(format!(
                    "{}: several staged backends share this name — drop-in not merged",
                    path.display()
                ));
            }
        }
    }

    /// A probe record: parse through the STRICT machine schema and stage it
    /// for attachment after all declarations are in.
    fn stage_probe(&mut self, path: &Path, stem: &str, text: &str) {
        let record = match parse_probe_record(text) {
            Ok(record) => record,
            Err(reason) => {
                self.warn(format!(
                    "{}: invalid probe record — not overlaid (delete the file to re-probe): {reason}",
                    path.display()
                ));
                return;
            }
        };
        self.pending_probes.push(PendingProbe {
            path: path.to_path_buf(),
            stem: stem.to_string(),
            observation: record.to_observation(stem),
        });
    }

    /// Attach every staged probe record against the FINAL declarations:
    /// the unique slot with the exact same name AND destination. Walk
    /// order, last-wins — a later directory's record deterministically
    /// supersedes an earlier one. A name or destination that no final
    /// declaration matches is skipped with a visible warning.
    fn attach_pending_probes(&mut self) {
        for pending in std::mem::take(&mut self.pending_probes) {
            let PendingProbe {
                path,
                stem,
                observation,
            } = pending;
            let slot = match self.find(&stem) {
                NameMatch::Unique(i) => &mut self.slots[i],
                NameMatch::Missing => {
                    self.warn(format!(
                        "{}: probe record names an unconfigured backend — ignored (delete the file)",
                        path.display()
                    ));
                    continue;
                }
                NameMatch::Ambiguous => {
                    self.warn(format!(
                        "{}: several staged backends share this name — probe record not attached",
                        path.display()
                    ));
                    continue;
                }
            };
            // Association is the exact declared destination — an endpoint-less
            // (embedded) backend is never overlaid, and a near-collision is a
            // different destination, not a match.
            let observed_at = BackendDestination::new(Some(observation.endpoint.clone()), None);
            let declared_at = BackendDestination::of(&slot.declaration);
            if declared_at != observed_at {
                let configured = slot.declaration.endpoint.clone();
                let probed = observation.endpoint.clone();
                self.warn(format!(
                    "{}: probe record's destination does not match the configured backend \
                     (configured={configured}, probed={probed}) — not overlaid",
                    path.display()
                ));
                continue;
            }
            slot.observation = Some(observation);
        }
    }

    /// Record the CLI `--backend-*` request.
    ///
    /// A destination request (`--backend-url` XOR `--backend-model-path` —
    /// exactly one, nonempty) defines an EXCLUSIVE backend: exactly one
    /// slot survives — the uniquely named existing one (its declaration and
    /// observation intact; whether the observation still applies is decided
    /// in [`Self::finish`]) or a brand-new slot with no declaration layer.
    ///
    /// A field-only request targets ONE slot in place: the named one (a
    /// name matching nothing is a hard, actionable error — `--backend-name`
    /// is both the edit target and this invocation's selection, never a
    /// silent no-op), else the slot the shared [`select_backend_slot`]
    /// picks — the SAME selector every consumer uses, so the edited backend
    /// IS the selected backend, never "index 0".
    ///
    /// Names are validated AGAIN afterwards — a request-created slot's name
    /// enters here.
    /// Returns the SLOT INDEX the request landed on (`None` when there was
    /// no request) so composing callers can align config-level selection
    /// with the target.
    fn apply_request(
        &mut self,
        over: Option<BackendOverride>,
        default_backend: Option<&str>,
    ) -> std::result::Result<Option<usize>, String> {
        // Probe attachment resolves against the FINAL directory
        // declarations BEFORE any exclusive pruning — a valid cache for a
        // disk-declared backend must not look "unconfigured" (and emit the
        // destructive delete/re-probe warning) merely because THIS
        // invocation selected another backend.
        self.attach_pending_probes();
        let Some(over) = over.filter(|o| !o.is_empty()) else {
            return Ok(None);
        };
        // Destination invariants: empty strings are malformed requests, and
        // a request cannot point two places at once.
        if over.endpoint.as_deref().is_some_and(str::is_empty) {
            return Err("--backend-url is empty — give a URL or omit the flag".into());
        }
        if over.model_path.as_deref().is_some_and(str::is_empty) {
            return Err("--backend-model-path is empty — give a path or omit the flag".into());
        }
        if over.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
            return Err(
                "--backend-model is empty — give a model or omit the flag (there is \
                 no implicit clear: the flattened route would serve \
                 server-decides while the receipt fell back to the stale \
                 declared model)"
                    .into(),
            );
        }
        if over.endpoint.is_some() && over.model_path.is_some() {
            return Err(
                "--backend-url and --backend-model-path are mutually exclusive — a \
                 backend has ONE destination (an HTTP endpoint, or an embedded \
                 artifact path)"
                    .into(),
            );
        }
        // Destination/kind coherence: an explicitly contradictory pair is an
        // operator error, not something to silently normalize away.
        if over.endpoint.is_some() && over.kind == Some(BackendKind::Embedded) {
            return Err(
                "--backend-url with --backend-kind embedded is contradictory — an \
                 embedded backend has no endpoint; use --backend-model-path"
                    .into(),
            );
        }
        if over.model_path.is_some() && over.kind.is_some_and(|k| k != BackendKind::Embedded) {
            return Err(format!(
                "--backend-model-path with --backend-kind {:?} is contradictory — a \
                 model_path destination is an embedded backend",
                over.kind.unwrap()
            ));
        }
        if over.model_path.is_some() && over.serving == Some(Serving::Multiplexer) {
            return Err(
                "--backend-model-path with --backend-serving multiplexer is \
                 contradictory — an embedded backend serves exactly one artifact \
                 (instance)"
                    .into(),
            );
        }
        self.requested = true;
        let has_destination = over.endpoint.is_some() || over.model_path.is_some();
        if has_destination {
            let name = over.name.clone().unwrap_or_else(|| "cli".to_string());
            let kept = match self.find(&name) {
                NameMatch::Unique(i) => self.slots.swap_remove(i),
                NameMatch::Missing => AssemblySlot::declared(BackendConfig {
                    name: name.clone(),
                    ..Default::default()
                }),
                NameMatch::Ambiguous => {
                    return Err(format!(
                        "--backend-* targets `{name}`, which several backends share — \
                         rename one"
                    ));
                }
            };
            self.slots = vec![kept];
            self.slots[0].request = Some(over);
            validate_backend_names(self.slots.iter().map(|s| &s.declaration))?;
            return Ok(Some(0));
        }
        {
            // Field-only targeting runs over the PROBE-INFORMED effective
            // view ([`AssemblySlot::observed_view`]) — the same facts the
            // final resolution selects on — so the slot the edit lands on
            // and the slot the session then selects cannot diverge over a
            // probed kind.
            let effective: Vec<BackendConfig> =
                self.slots.iter().map(AssemblySlot::observed_view).collect();
            let idx = match over.name.as_deref() {
                Some(n) => match self.find(n) {
                    NameMatch::Unique(i) => {
                        if !backend_is_routable(&effective[i]) {
                            return Err(format!(
                                "--backend-name `{n}` names a backend with neither an \
                                 endpoint nor a model_path — a field-only --backend-* \
                                 cannot route it; give it a destination \
                                 (--backend-url / --backend-model-path) or fix the \
                                 backend"
                            ));
                        }
                        i
                    }
                    NameMatch::Missing => {
                        let configured: Vec<&str> = self
                            .slots
                            .iter()
                            .map(|s| s.declaration.name.as_str())
                            .collect();
                        return Err(format!(
                            "--backend-name `{n}` matches no configured backend \
                             (configured: {configured:?}) — a field-only --backend-* \
                             edits an existing backend; add --backend-url to define \
                             a new one"
                        ));
                    }
                    NameMatch::Ambiguous => {
                        return Err(format!(
                            "--backend-* targets `{n}`, which several backends share — \
                             rename one"
                        ));
                    }
                },
                None => {
                    let declarations: Vec<&BackendConfig> = effective.iter().collect();
                    match select_backend_slot(&declarations, default_backend) {
                        SlotSelection::Slot(i) => i,
                        // A field-only request supplies no destination, so
                        // editing the explicitly selected but destination-less
                        // backend could not make it routable — and editing any
                        // OTHER backend would desert the explicit selection.
                        SlotSelection::ExplicitlyUnroutable { name } => {
                            return Err(format!(
                                "--backend-* targets `{name}` (named by $NEWT_PROVIDER or \
                                 default_backend), which has neither an endpoint nor a \
                                 model_path — a field-only --backend-* cannot route it; \
                                 give it a destination (--backend-url / \
                                 --backend-model-path) or fix the backend"
                            ));
                        }
                        SlotSelection::ExplicitlyUnmatched { name } => {
                            return Err(format!(
                                "--backend-* would apply to the selected backend, but \
                                 $NEWT_PROVIDER/default_backend names `{name}`, which \
                                 matches no configured backend (it may name a provider, \
                                 which --backend-* cannot edit) — fix the selector or \
                                 name a backend with --backend-name"
                            ));
                        }
                        SlotSelection::None => {
                            return Err("--backend-* has no backend to apply to — nothing \
                                 configured is routable; name one with --backend-name \
                                 or define one with --backend-url"
                                .into());
                        }
                    }
                }
            };
            // A field-only kind change must agree with the destination the
            // target already has — refused ATOMICALLY here, never recorded
            // and then silently normalized away in composition.
            if let Some(kind) = over.kind {
                let target = &effective[idx];
                if kind == BackendKind::Embedded && !target.endpoint.is_empty() {
                    return Err(format!(
                        "--backend-kind embedded on `{}` is contradictory — its \
                         destination is an HTTP endpoint; retarget with \
                         --backend-model-path or pick an HTTP kind",
                        target.name
                    ));
                }
                if kind != BackendKind::Embedded
                    && target.endpoint.is_empty()
                    && target.model_path.as_deref().is_some_and(|p| !p.is_empty())
                {
                    return Err(format!(
                        "--backend-kind {kind:?} on `{}` is contradictory — its \
                         destination is an embedded model_path; retarget with \
                         --backend-url or keep kind embedded",
                        target.name
                    ));
                }
            }
            // A field-only serving change must agree with the target's
            // destination, exactly like kind: an embedded (model_path)
            // backend serves one artifact — refused ATOMICALLY, never
            // recorded and then silently normalized away.
            if over.serving == Some(Serving::Multiplexer) {
                let target = &effective[idx];
                if target.endpoint.is_empty()
                    && target.model_path.as_deref().is_some_and(|p| !p.is_empty())
                {
                    return Err(format!(
                        "--backend-serving multiplexer on `{}` is contradictory — an \
                         embedded (model_path) backend serves exactly one artifact \
                         (instance); retarget with --backend-url for a multiplexer",
                        target.name
                    ));
                }
            }
            // Selection PARITY for the unnamed edit: the request itself can
            // reorder the shared precedence (a kind edit adds/removes the
            // prefer-OpenAI property), so re-run the selector over the
            // POST-request view and require it to still pick the edited
            // slot — otherwise the backend the edit landed on and the
            // backend the session then selects would diverge. A
            // destabilizing edit must name its target.
            if over.name.is_none() {
                let mut post: Vec<BackendConfig> = effective.clone();
                over.overlay(&mut post[idx]);
                let post_refs: Vec<&BackendConfig> = post.iter().collect();
                match select_backend_slot(&post_refs, default_backend) {
                    SlotSelection::Slot(i) if i == idx => {}
                    _ => {
                        return Err(format!(
                            "--backend-* would edit `{}` (the currently selected \
                             backend), but the edit changes which backend the shared \
                             precedence selects — name the target explicitly with \
                             --backend-name",
                            self.slots[idx].declaration.name
                        ));
                    }
                }
            }
            self.slots[idx].request = Some(over);
            validate_backend_names(self.slots.iter().map(|s| &s.declaration))?;
            Ok(Some(idx))
        }
    }

    /// Compose the layers: per slot, the effective [`BackendConfig`]
    /// (declaration → retained observation → request) and the
    /// [`BackendResolutionReceipt`], aligned 1:1 by index.
    ///
    /// * A requested destination CHANGE clears the cached observation —
    ///   truth observed at one destination never rides to another; an
    ///   identical requested destination retains it.
    /// * The binding: an explicit `--backend-card` rebinds at the
    ///   post-request destination to the requested-or-DECLARED model (never
    ///   a probed one); otherwise the declared binding stands untouched —
    ///   including under a model-only or endpoint-only request, whose
    ///   visibility is a typed downstream decision, not an erasure here.
    fn finish(mut self) -> (Vec<BackendConfig>, Vec<BackendResolutionReceipt>) {
        self.attach_pending_probes();
        let mut backends = Vec::with_capacity(self.slots.len());
        let mut receipts = Vec::with_capacity(self.slots.len());
        for slot in self.slots {
            let declaration = DeclaredBackend::of(&slot.declaration);
            let request = slot.request.as_ref().map(BackendRequest::from_override);
            let destination = request
                .as_ref()
                .map(|r| r.destination_over(&declaration.destination))
                .unwrap_or_else(|| declaration.destination.clone());
            let observation = slot
                .observation
                .filter(|_| destination == declaration.destination);

            let mut backend = slot.declaration;
            if let Some(obs) = &observation {
                overlay_observation(&mut backend, obs);
            }
            if let Some(over) = &slot.request {
                over.overlay(&mut backend);
                // Tier defaulting belongs to the EXCLUSIVE destination
                // request only (a fresh/retargeted backend must actually
                // serve). A field-only edit never invents tiers: an
                // intentionally empty `tiers = []` declaration stays empty.
                let exclusive = over.endpoint.is_some() || over.model_path.is_some();
                if exclusive && backend.tiers.is_empty() {
                    backend.tiers = vec![Tier::Fast, Tier::Standard, Tier::Complex, Tier::Review];
                }
            }
            // Destination/kind coherence — the SAME normalization the
            // targeting preview applies ([`normalize_destination_kind`]).
            // Explicitly CONTRADICTORY requests were rejected in
            // `apply_request`; this normalizes residual declared/probed kind
            // after a destination changed around it.
            normalize_destination_kind(&mut backend);

            let binding = match &request {
                Some(req) if req.card.is_some() => crate::model_card::CardBindingSeed {
                    card: req.card.clone(),
                    bound_model: req.model.clone().or_else(|| declaration.model.clone()),
                    bound_destination: destination.clone(),
                },
                _ => crate::model_card::CardBindingSeed {
                    card: declaration.card.clone(),
                    bound_model: declaration.model.clone(),
                    bound_destination: declaration.destination.clone(),
                },
            };
            receipts.push(BackendResolutionReceipt {
                declaration,
                request,
                observation,
                binding,
            });
            backends.push(backend);
        }
        (backends, receipts)
    }
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

    /// Apply to a resolved config — the INFALLIBLE compatibility surface:
    /// delegates to [`BackendOverride::try_apply`] (the one invariant-owning
    /// composer) and, when the request is refused (both/empty destinations,
    /// a contradictory kind, a named backend that does not exist, duplicate
    /// names), warns and leaves the config untouched. It can no longer
    /// violate the XOR/nonempty/shared-selector/named-miss semantics the
    /// assembly enforces.
    pub fn apply(&self, cfg: &mut Config) {
        if let Err(e) = self.try_apply(cfg) {
            tracing::warn!(error = %e, "--backend-* override not applied");
        }
    }

    /// Apply to a resolved config through the SAME backend-assembly path
    /// `Config::resolve_runtime` uses — one composer, one set of
    /// invariants:
    ///
    /// * a destination request (`--backend-url` XOR `--backend-model-path`,
    ///   nonempty, kind-coherent) defines an **exclusive** backend that
    ///   REPLACES all others;
    /// * a field-only request edits the NAMED backend (a name matching
    ///   nothing is an error, never a silent no-op) or, unnamed, the
    ///   backend the shared selection precedence picks — never "index 0";
    /// * destination/kind coherence is normalized exactly as in
    ///   `resolve_runtime` (a model_path route is Embedded; an endpoint
    ///   route never retains Embedded).
    ///
    /// On error the config is byte-for-byte untouched.
    ///
    /// # Errors
    /// Duplicate/empty backend names; both or empty destinations; a
    /// contradictory destination/kind pair; a named or explicitly selected
    /// target that does not exist or cannot be routed.
    pub fn try_apply(&self, cfg: &mut Config) -> std::result::Result<(), String> {
        if self.is_empty() {
            return Ok(());
        }
        let original = cfg.backends.clone();
        let mut assembly = match BackendAssembly::new(std::mem::take(&mut cfg.backends)) {
            Ok(assembly) => assembly,
            Err(e) => {
                cfg.backends = original;
                return Err(e);
            }
        };
        let default_backend = cfg.default_backend.clone();
        let applied = assembly.apply_request(Some(self.clone()), default_backend.as_deref());
        let (backends, _receipts) = assembly.finish();
        match applied {
            Ok(target) => {
                cfg.backends = backends;
                // An explicit `--backend-*` flag is operator configuration —
                // the session is no longer on the bare compiled-in fallback.
                cfg.backend_fallback = false;
                // The one selection-follows-the-request rule, shared with
                // the runtime composers (the binary additionally sets
                // $NEWT_PROVIDER).
                pin_requested_selection(cfg, Some(self), target);
                Ok(())
            }
            Err(e) => {
                cfg.backends = original;
                Err(e)
            }
        }
    }

    /// Copy every set field onto `backend` (leaving unset fields untouched).
    /// A requested destination REPLACES the destination axis whole: both
    /// effective fields are cleared before the requested one installs, so an
    /// HTTP→embedded (or embedded→HTTP) retarget cannot retain the opposite
    /// field and leave the backend pointing two places at once.
    fn overlay(&self, backend: &mut BackendConfig) {
        if self.endpoint.is_some() || self.model_path.is_some() {
            backend.endpoint = String::new();
            backend.model_path = None;
        }
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

/// Write one backend as a per-file drop-in `<config dir>/backends/<name>.toml`
/// (#1140, epic #1126) — the shape the backend assembly's drop-in merge reads
/// back. The
/// canonical writer for `newt init` / `newt setup`: one endpoint, one file,
/// provenance-stamped by the caller. Returns the written path.
pub fn write_backend_dropin(
    config_path: &std::path::Path,
    backend: &BackendConfig,
) -> std::result::Result<std::path::PathBuf, String> {
    let config_destination = crate::atomic_fs::ResolvedPath::resolve(config_path)
        .map_err(|error| format!("resolve config destination: {error:#}"))?;
    let _lock = crate::atomic_fs::acquire_lock(&config_destination.lock_path())
        .map_err(|error| format!("lock {}: {error:#}", config_path.display()))?;
    write_backend_dropin_unlocked(config_path, backend)
}

fn write_backend_dropin_unlocked(
    config_path: &std::path::Path,
    backend: &BackendConfig,
) -> std::result::Result<std::path::PathBuf, String> {
    if backend.name.trim().is_empty() {
        return Err("backend drop-in needs a name (it becomes the filename)".into());
    }
    let dir = config_path.with_file_name("backends");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join(format!("{}.toml", backend.name));
    let destination = crate::atomic_fs::ResolvedPath::resolve(&path)
        .map_err(|e| format!("resolve {}: {e:#}", path.display()))?;
    // Every canonical operator write is TAGGED `operator_v1` —
    // UNCONDITIONALLY, injected at the file boundary by the ONE shared
    // renderer ([`render_operator_backend_dropin`]). `BackendConfig`
    // carries no `record` field at all, so there is no in-memory tag to
    // launder through this channel; probe persistence has its own API
    // ([`persist_probe_observation`]).
    let body = render_operator_backend_dropin(backend)?;
    destination
        .atomic_write(body.as_bytes())
        .map_err(|e| format!("write {}: {e:#}", path.display()))?;
    Ok(path)
}

/// Who owns a backend drop-in FILE — the public ownership view for setup /
/// panel surfaces ("may I edit this file?", "is this a probe cache?"),
/// without exposing the raw on-disk tag vocabulary. Ownership is about the
/// FILE: [`crate::BackendConfig`] deliberately carries no tag, so this is
/// decided from raw text ([`classify_backend_dropin`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DropinOwnership {
    /// Operator-owned: explicitly tagged as operator configuration, or
    /// untagged (hand-authored files and every legacy operator writer).
    /// Panels may edit it; the probe writeback never touches it.
    Operator,
    /// Machine-owned probe record: explicitly tagged, or the unambiguous
    /// legacy probe cache. The runtime rewrites it wholesale; delete (or
    /// [`claim_backend_dropin_as_operator`]) to take it over.
    Probe,
}

/// Classify a backend drop-in file's raw text — the SAME ownership decision
/// the loader and [`persist_probe_observation`] make, exposed for panel /
/// setup surfaces. Ownership only: a probe-owned file that later fails the
/// strict probe schema is still probe-owned (and will be skipped, not
/// reinterpreted, by the loader).
///
/// # Errors
/// Malformed TOML, and the legacy ambiguity (the exact old newt-adopt probe
/// marker beside binding/operator evidence) with both remediations.
pub fn classify_backend_dropin(text: &str) -> std::result::Result<DropinOwnership, String> {
    match disk_record_tag(text)? {
        Some(RecordTag::ProbeV1) => Ok(DropinOwnership::Probe),
        Some(RecordTag::OperatorV1) => Ok(DropinOwnership::Operator),
        None => {
            let backend = toml::from_str::<BackendConfig>(text).map_err(|e| e.to_string())?;
            match classify_untagged_dropin(&backend, text)? {
                DropinOwner::Operator => Ok(DropinOwnership::Operator),
                DropinOwner::Probe => Ok(DropinOwnership::Probe),
            }
        }
    }
}

/// The canonical operator drop-in body: the ownership stamp as the first
/// top-level key (always valid TOML), then the backend's serialization,
/// byte-identical to serializing `backend` alone. The ONE producer of
/// operator-record bytes — [`write_backend_dropin`] writes exactly this,
/// and a panel/setup surface that builds file bodies itself must use it
/// rather than hand-roll the stamp.
///
/// # Errors
/// Serialization failure, as a human-readable string.
pub fn render_operator_backend_dropin(
    backend: &BackendConfig,
) -> std::result::Result<String, String> {
    let serialized = toml::to_string(backend).map_err(|e| format!("serialize backend: {e}"))?;
    Ok(format!("record = \"operator_v1\"\n{serialized}"))
}

/// Claim a drop-in file as OPERATOR configuration — retag a probe record
/// (or tag an untagged file) **preserving comments, key order, and every
/// key newt does not model**, unlike a serde round-trip. The panel's "keep
/// this probed result as my configuration" edit; idempotent on a file that
/// is already operator-tagged.
///
/// # Errors
/// Text that is not valid TOML.
pub fn claim_backend_dropin_as_operator(text: &str) -> std::result::Result<String, String> {
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("backend drop-in is not valid TOML: {e}"))?;
    let root = doc.as_table_mut();
    match root.get_mut("record") {
        // Retag IN PLACE, keeping the existing value's decor — the trailing
        // comment on `record = "probe_v1"  # ownership note` is the
        // operator's annotation, and a blunt replacement would drop it.
        Some(item) if item.is_value() => {
            let value = item.as_value_mut().expect("checked is_value");
            let decor = value.decor().clone();
            *value = toml_edit::Value::from("operator_v1");
            *value.decor_mut() = decor;
        }
        // A `[record]` table or `[[record]]` array is NOT an ownership tag
        // — refuse rather than overwrite someone's data with a stamp.
        Some(_) => {
            return Err(
                "this drop-in has a `[record]` table/array where the ownership tag \
                 would go — refusing to overwrite it; rename or remove that table \
                 first, then claim the file"
                    .to_string(),
            );
        }
        // Absent: stamp a fresh top-level key.
        None => {
            root.insert("record", toml_edit::value("operator_v1"));
        }
    }
    Ok(doc.to_string())
}

/// What a session probe/adoption OBSERVED — the ONLY thing the runtime may
/// persist about a backend. Typed so an unpersistable fact is
/// unrepresentable: only an [`ProbedServing::Instance`] carries a model
/// (one artifact = backend truth); a multiplexer's per-session pick and an
/// unestablished axis have no model field to persist at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeObservation {
    /// The configured backend the observation is about (the drop-in filename).
    pub name: String,
    /// The endpoint the probe actually spoke to — the association key.
    pub endpoint: String,
    /// The detected wire protocol, when the probe established one.
    pub kind: Option<BackendKind>,
    /// The detected OpenAI HTTP surface, when probed.
    pub api: Option<OpenAiApi>,
    /// The observed serving principal.
    pub serving: ProbedServing,
}

/// The serving principal a probe observed — the typed gate on model
/// persistence (see [`ProbeObservation`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbedServing {
    /// A single-artifact server; its model IS backend truth and may persist.
    Instance { model: Option<String> },
    /// A multi-model server; the adopted model is a per-session pick and has
    /// no field here to persist through.
    Multiplexer,
    /// Serving was not established — nothing about the axis persists.
    Unknown,
}

impl ProbeObservation {
    /// The `(serving, model)` axis pair this observation's typed principal
    /// flattens to — the ONLY conversion, so "model iff Instance" holds by
    /// construction everywhere the observation is applied or serialized.
    #[must_use]
    pub fn serving_axis(&self) -> (Option<Serving>, Option<String>) {
        match &self.serving {
            ProbedServing::Instance { model } => (Some(Serving::Instance), model.clone()),
            ProbedServing::Multiplexer => (Some(Serving::Multiplexer), None),
            ProbedServing::Unknown => (None, None),
        }
    }
}

/// The `record = "probe_v1"` machine record an observation serializes as —
/// only observed fields, never card/capability/auth/tiers/managed/host.
/// Pure; [`persist_probe_observation`] owns the IO.
fn probe_machine_record(observation: &ProbeObservation) -> ProbeRecordV1 {
    let (serving, model) = observation.serving_axis();
    ProbeRecordV1 {
        name: Some(observation.name.clone()),
        endpoint: observation.endpoint.clone(),
        kind: observation.kind,
        api: observation.api,
        serving,
        model,
        tiers: Vec::new(),
        record: Some(RecordTag::ProbeV1),
        provenance: Some(ProbeProvenanceV1 {
            source: Some(format!(
                "newt adopt v{} (probe_v1 overlay; delete this file to reset)",
                crate::build_info::VERSION_WITH_COMMIT
            )),
            probed: Some(chrono::Local::now().format("%Y-%m-%d").to_string()),
            derived_serving: serving.map(|_| true),
        }),
    }
}

/// Who an UNTAGGED drop-in belongs to (files written before [`RecordTag`]
/// existed).
#[derive(Debug)]
enum DropinOwner {
    Operator,
    Probe,
}

/// The fully anchored EXACT marker the old runtime writeback stamped:
/// `newt adopt v{version} (probed; delete this file to reset)` — prefix and
/// suffix both anchored, nonempty version between. A near-prefix, a
/// near-suffix, or any custom source is NOT this marker.
fn is_legacy_adopt_probe_marker(source: &str) -> bool {
    source
        .strip_prefix("newt adopt v")
        .and_then(|rest| rest.strip_suffix(" (probed; delete this file to reset)"))
        .is_some_and(|version| !version.is_empty())
}

/// Classify an untagged backend drop-in. **Untagged is Operator by
/// default** — the hand-authored file, the old `newt setup v{…}` /
/// `newt init v{…}` / provider-preset (`newt setup v{…} (preset {name})`)
/// writers, and every custom or probe-stamped source alike: a generic
/// `provenance.probed` timestamp proves nothing (operator writers stamped
/// one too) and is never branched on.
///
/// The ONE exception is the fully anchored exact historical newt-adopt
/// probe marker ([`is_legacy_adopt_probe_marker`]). A file carrying exactly
/// that marker is judged on its RAW key shape (`text`, through the strict
/// [`ProbeRecordV1`] whitelist — the permissive [`BackendConfig`] parse
/// silently DROPS unknown evidence and must not decide this):
///
/// * the strict MODEL-LESS probe shape (endpoint/kind/api/serving only,
///   empty `tiers`, no unknown keys top-level or under `[provenance]`) →
///   the legacy probe cache, [`DropinOwner::Probe`] — overlaid under
///   today's probe rules and migrated on next writeback;
/// * ANYTHING else beside the marker — a `model` (whatever the serving
///   axis), a `card`, auth/tiers/managed/…, or any UNKNOWN key (evidence
///   the old writer never produced) — is genuinely ambiguous: hard-error
///   with both remediations rather than guess.
fn classify_untagged_dropin(
    b: &BackendConfig,
    text: &str,
) -> std::result::Result<DropinOwner, String> {
    let source = b
        .provenance
        .as_ref()
        .and_then(|p| p.source.as_deref())
        .unwrap_or("");
    if !is_legacy_adopt_probe_marker(source) {
        return Ok(DropinOwner::Operator);
    }
    let strict = toml::from_str::<ProbeRecordV1>(text);
    if let Ok(record) = &strict {
        if record.model.is_none() && record.tiers.is_empty() {
            return Ok(DropinOwner::Probe);
        }
    }
    let carried = match (b.model.is_some(), b.card.is_some(), strict.is_err()) {
        (true, true, _) => "a model and a card",
        (true, false, _) => "a model",
        (false, true, _) => "a card",
        (false, false, true) => "keys outside the old probe cache's raw shape",
        (false, false, false) => "operator fields beside the probe marker",
    };
    Err(format!(
        "this backend drop-in carries the old newt-adopt probe marker but also \
         {carried} — written by an older newt, its declarations cannot be \
         attributed: as an operator record (A) they replace the configured \
         backend wholesale; as a probe overlay (B) they are per-session residue \
         that must be discarded. Refusing to guess — delete the file to \
         re-probe, or add `record = \"operator_v1\"` to claim it as \
         configuration."
    ))
}

/// A probe record may carry ONLY what a probe can observe: `endpoint` (the
/// association key, nonempty), `kind`, `api`, `serving`, and `model` iff
/// `serving = "instance"`. Enforced on load AND around every write, so a
/// hand-edited or corrupted `probe_v1` file cannot smuggle operator fields
/// through the machine-owned channel.
fn validate_probe_record(r: &ProbeRecordV1) -> std::result::Result<(), String> {
    if r.endpoint.trim().is_empty() {
        return Err("probe record has no endpoint (the association key)".to_string());
    }
    if r.model.is_some() && r.serving != Some(Serving::Instance) {
        return Err(
            "probe record carries a model without serving = \"instance\" — only an \
             instance's model is backend truth"
                .to_string(),
        );
    }
    // Operator-owned keys are UNREPRESENTABLE in [`ProbeRecordV1`] (denied
    // at parse). The one legacy leftover the schema tolerates on read is an
    // empty `tiers = []`; a NONEMPTY one is operator configuration.
    if !r.tiers.is_empty() {
        return Err(
            "probe record carries operator-owned field `tiers` — a probe overlay may \
             hold only endpoint/kind/api/serving (plus an instance's model)"
                .to_string(),
        );
    }
    Ok(())
}

/// The PRIVATE raw header of a backend drop-in file — the only place the
/// `record` ownership key is read. [`BackendConfig`] deliberately does NOT
/// carry the tag: ownership is a property of the FILE, decided at the disk
/// boundary, and a tag smuggled through the in-memory config type was how a
/// probe record could try to launder itself through the operator writer.
#[derive(Deserialize)]
struct DiskRecordHeader {
    #[serde(default)]
    record: Option<RecordTag>,
}

/// The `record` tag of a drop-in's raw text, if any. Unknown sibling keys
/// are ignored — this reads the header, nothing else.
fn disk_record_tag(text: &str) -> std::result::Result<Option<RecordTag>, String> {
    toml::from_str::<DiskRecordHeader>(text)
        .map(|h| h.record)
        .map_err(|e| e.to_string())
}

/// The strict machine-record schema for a probe drop-in — a
/// `deny_unknown_fields` mirror of the probe-legal subset of
/// [`BackendConfig`]. [`BackendConfig`] itself tolerates unknown TOML keys
/// (forward compatibility for operator files), which means parsing a probe
/// record through it silently DROPS whatever a hand-edit smuggled in —
/// [`validate_probe_record`] can only reject what survives the parse. Probe
/// records are machine-owned, so they get the opposite contract: an unknown
/// key is a hard parse error, an operator-owned key doubly so (it is
/// unknown HERE by construction).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeRecordV1 {
    /// The filename stem is authoritative, but the body may repeat it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default)]
    endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<BackendKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api: Option<OpenAiApi>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    serving: Option<Serving>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    /// Accepted on READ only: the old writeback serialized its
    /// `BackendConfig` patch verbatim, so genuine legacy probe caches carry
    /// a literal `tiers = []`. [`validate_probe_record`] still rejects a
    /// NONEMPTY value; the writer never emits the key again.
    #[serde(default, skip_serializing)]
    tiers: Vec<Tier>,
    /// Absent on a legacy (pre-[`RecordTag`]) probe cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    record: Option<RecordTag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    provenance: Option<ProbeProvenanceV1>,
}

/// Strict mirror of [`BackendProvenance`] for probe records — the parent is
/// permissive (operator files get forward compatibility), so reusing it
/// here would let unknown NESTED keys deserialize away and the strictness
/// of [`ProbeRecordV1`] would stop one level deep.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeProvenanceV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    probed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    derived_serving: Option<bool>,
}

/// Parse the raw text of a probe-owned drop-in through the strict
/// [`ProbeRecordV1`] schema. Callers still run [`validate_probe_record`] on
/// the result (endpoint nonempty, model iff instance) — this layer's job is
/// the key set, which the permissive [`BackendConfig`] parse cannot police.
fn parse_probe_record(text: &str) -> std::result::Result<ProbeRecordV1, String> {
    let r: ProbeRecordV1 =
        toml::from_str(text).map_err(|e| format!("not a valid probe record: {e}"))?;
    validate_probe_record(&r).map(|()| r)
}

impl ProbeRecordV1 {
    /// The typed observation a validated record attests — `name` supplied by
    /// the caller (the filename stem is authoritative for drop-ins).
    fn to_observation(&self, name: &str) -> ProbeObservation {
        ProbeObservation {
            name: name.to_string(),
            endpoint: self.endpoint.clone(),
            kind: self.kind,
            api: self.api,
            serving: match (self.serving, &self.model) {
                (Some(Serving::Instance), model) => ProbedServing::Instance {
                    model: model.clone(),
                },
                (Some(Serving::Multiplexer), _) => ProbedServing::Multiplexer,
                (None, _) => ProbedServing::Unknown,
            },
        }
    }
}

/// The visible outcome of a probe writeback — persistence is explicitly
/// owned, so "did not write" states are typed, never silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeWriteback {
    /// The probe_v1 record was created or updated at this path.
    Written(std::path::PathBuf),
    /// The same-name path is operator-owned (`operator_v1` or untagged) —
    /// its bytes and comments were left untouched.
    SkippedOperatorOwned(std::path::PathBuf),
    /// No user config dir, or an unnamed backend — nothing to persist to.
    NotWritten,
}

/// Persist a probe observation as `~/.newt/backends/<name>.toml` (or under
/// `$NEWT_CONFIG_DIR`) — never into the main `config.toml`. Reset = delete
/// that one file.
///
/// Creates or updates ONLY probe-owned files. An existing same-name file
/// that is operator-owned (tagged `operator_v1`, or untagged and classified
/// operator — same classifier as the loader) is returned as
/// [`ProbeWriteback::SkippedOperatorOwned`] byte-for-byte untouched — the
/// runtime never rewrites operator configuration. An unambiguous LEGACY
/// probe cache (untagged, exact old adopt marker, probe-shaped) is treated
/// as the prior probe record and MIGRATES to tagged `probe_v1` on this
/// write; the genuinely ambiguous legacy file hard-errors with both
/// remediations, exactly as on load. An update re-serializes the probe
/// schema, carrying forward the prior probe file's `kind`/`api` only when
/// its endpoint equals this observation's — `serving`/`model` are NEVER
/// carried, so an Instance-observed model is REMOVED the moment a later
/// observation sees a multiplexer (or nothing).
///
/// # Errors
/// Lock/read/parse/serialize/write failures — and the legacy ambiguity —
/// as human-readable strings.
pub fn persist_probe_observation(
    observation: &ProbeObservation,
) -> std::result::Result<ProbeWriteback, String> {
    if observation.name.trim().is_empty() {
        return Ok(ProbeWriteback::NotWritten);
    }
    let Some(config_path) = Config::user_config_path() else {
        return Ok(ProbeWriteback::NotWritten);
    };
    let config_destination = crate::atomic_fs::ResolvedPath::resolve(&config_path)
        .map_err(|error| format!("resolve config destination: {error:#}"))?;
    let _lock = crate::atomic_fs::acquire_lock(&config_destination.lock_path())
        .map_err(|error| format!("lock {}: {error:#}", config_path.display()))?;
    let dir = config_path.with_file_name("backends");
    let path = dir.join(format!("{}.toml", observation.name));
    let destination = crate::atomic_fs::ResolvedPath::resolve(&path)
        .map_err(|e| format!("resolve {}: {e:#}", path.display()))?;
    let mut merged = probe_machine_record(observation);
    if destination.as_path().is_file() {
        let text = std::fs::read_to_string(destination.as_path())
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        // Ownership is decided the SAME way the loader decides it — the raw
        // `record` header, else the legacy classifier — or an unambiguous
        // legacy probe cache would permanently block refresh.
        let owned_by_probe =
            match disk_record_tag(&text).map_err(|e| format!("parse {}: {e}", path.display()))? {
                Some(RecordTag::ProbeV1) => true,
                Some(RecordTag::OperatorV1) => false,
                None => {
                    let prior = toml::from_str::<BackendConfig>(&text)
                        .map_err(|e| format!("parse {}: {e}", path.display()))?;
                    match classify_untagged_dropin(&prior, &text) {
                        Ok(DropinOwner::Probe) => true,
                        Ok(DropinOwner::Operator) => false,
                        Err(reason) => return Err(format!("{}: {reason}", path.display())),
                    }
                }
            };
        if !owned_by_probe {
            return Ok(ProbeWriteback::SkippedOperatorOwned(path));
        }
        let prior = parse_probe_record(&text).map_err(|e| {
            format!(
                "{}: existing probe record is invalid ({e}) — delete it to re-probe",
                path.display()
            )
        })?;
        // Prior fields may be reused only for the SAME endpoint — an
        // endpoint change means every prior observation was about some
        // other server. serving/model are NEVER carried forward at all:
        // stale principal evidence must not be re-stamped under a fresh
        // probe date (an Unknown/model-less observation writes an
        // empty-principal record, it does not refresh the old one).
        if prior.endpoint == observation.endpoint {
            merged.kind = merged.kind.or(prior.kind);
            merged.api = merged.api.or(prior.api);
        }
    }
    validate_probe_record(&merged)
        .map_err(|e| format!("refusing to write an invalid probe record: {e}"))?;
    let body = toml::to_string(&merged).map_err(|e| format!("serialize probe record: {e}"))?;
    destination
        .atomic_write(body.as_bytes())
        .map_err(|e| format!("write {}: {e:#}", path.display()))?;
    Ok(ProbeWriteback::Written(path))
}

/// Deprecated compatibility shim for the pre-#1819 writeback API, which
/// took a raw [`BackendConfig`] patch and merged it into the drop-in. The
/// typed channel is [`persist_probe_observation`]; this shim converts the
/// patch — and REFUSES, before any write, a patch the typed channel cannot
/// represent, instead of reporting a lossy conversion as success:
///
/// * a `model` without `serving = "instance"` (a per-session pick is not
///   persistable backend truth);
/// * any operator-owned field (card, capability, auth, tiers, managed,
///   host, coexist, ram_gib, engine, model_path).
///
/// An operator-owned same-name file is likewise an ERROR naming the path —
/// the old API's `Ok(Some(path))` meant "persisted", and silently not
/// persisting is not compatibility. `Ok(None)` is returned only for the
/// true nothing-to-do cases (unnamed backend, no user config dir).
#[deprecated(note = "use persist_probe_observation — probe persistence is typed (#1819)")]
pub fn writeback_probed_backend(
    patch: &BackendConfig,
) -> std::result::Result<Option<std::path::PathBuf>, String> {
    if patch.model.is_some() && patch.serving != Some(Serving::Instance) {
        return Err(
            "probe writeback carries a model without serving = \"instance\" — only an \
             instance's model is backend truth; use persist_probe_observation"
                .to_string(),
        );
    }
    let operator_owned: &[(&str, bool)] = &[
        ("card", patch.card.is_some()),
        ("capability", patch.capability.is_some()),
        ("api_key_env", patch.api_key_env.is_some()),
        ("api_key_file", patch.api_key_file.is_some()),
        ("managed", patch.managed.is_some()),
        ("host", patch.host.is_some()),
        ("coexist", patch.coexist.is_some()),
        ("ram_gib", patch.ram_gib.is_some()),
        ("engine", patch.engine.is_some()),
        ("model_path", patch.model_path.is_some()),
        ("tiers", !patch.tiers.is_empty()),
    ];
    if let Some((field, _)) = operator_owned.iter().find(|(_, present)| *present) {
        return Err(format!(
            "probe writeback carries operator-owned field `{field}` — a probe record may \
             hold only endpoint/kind/api/serving (plus an instance's model); use \
             write_backend_dropin for operator configuration"
        ));
    }
    let serving = match patch.serving {
        Some(Serving::Instance) => ProbedServing::Instance {
            model: patch.model.clone(),
        },
        Some(Serving::Multiplexer) => ProbedServing::Multiplexer,
        None => ProbedServing::Unknown,
    };
    let observation = ProbeObservation {
        name: patch.name.clone(),
        endpoint: patch.endpoint.clone(),
        kind: patch.kind,
        api: patch.api,
        serving,
    };
    match persist_probe_observation(&observation)? {
        ProbeWriteback::Written(path) => Ok(Some(path)),
        ProbeWriteback::SkippedOperatorOwned(path) => Err(format!(
            "{}: the same-name drop-in is operator-owned — the probe record was NOT \
             written (delete the file, or keep it and stop probing this backend)",
            path.display()
        )),
        ProbeWriteback::NotWritten => Ok(None),
    }
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
        let text = match std::fs::read_to_string(destination.as_path()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(NewtError::Io(error)),
        };
        let updated = Self::with_net_host(&text, host)?;
        destination
            .atomic_write(updated.as_bytes())
            .map_err(|error| NewtError::Config(format!("write {}: {error:#}", path.display())))
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
    "refresh_token",
    "secret",
    "client_secret",
    "password",
    "passphrase",
    "key",
    "credential",
    "credentials",
    "signature",
    "sig",
    "x_amz_signature",
    "x_goog_signature",
    "shared_access_signature",
];

/// CLI flags (case-insensitive) whose value is a secret when redacting MCP
/// `args` — both the `--flag=VALUE` and `--flag VALUE` forms (#1301).
const SENSITIVE_ARG_FLAGS: &[&str] = &[
    "-b",
    "-u",
    "--auth",
    "--authorization",
    "--cookie",
    "--oauth2-bearer",
    "--proxy-user",
    "--user",
    "--token",
    "--access-token",
    "--refresh-token",
    "--api-key",
    "--client-secret",
    "--password",
    "--passphrase",
    "--secret",
    "--key",
    "--credential",
    "--credentials",
    "--signature",
    "--sig",
];

/// Decode URL percent escapes for secret-key classification. Invalid escapes
/// stay literal: malformed input must not panic, and valid encoded spellings
/// such as `client%5Fsecret` must not bypass redaction.
fn percent_decode_for_classification(value: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Normalize common credential-key spellings before classification. Query
/// keys are percent-decoded first and separators collapse to underscores, so
/// `client-secret`, `client_secret`, and `client%5Fsecret` share one policy.
fn normalized_credential_key(value: &str) -> String {
    let decoded = percent_decode_for_classification(value);
    let mut normalized = String::with_capacity(decoded.len());
    let mut separator = false;
    for ch in decoded.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
            separator = false;
        } else if !normalized.is_empty() && !separator {
            normalized.push('_');
            separator = true;
        }
    }
    while normalized.ends_with('_') {
        normalized.pop();
    }
    normalized
}

fn is_sensitive_credential_key(value: &str) -> bool {
    let normalized = normalized_credential_key(value);
    SENSITIVE_QUERY_KEYS.contains(&normalized.as_str())
        || normalized.ends_with("_token")
        || normalized.ends_with("_secret")
        || normalized.ends_with("_password")
        || normalized.ends_with("_credential")
        || normalized.ends_with("_signature")
}

/// Credential-bearing HTTP field names accepted by common command-line HTTP
/// clients. Vendor headers conventionally add `X-` to the same credential key,
/// so classify the suffix as well as the complete name.
fn is_sensitive_header_name(value: &str) -> bool {
    let normalized = normalized_credential_key(value);
    normalized == "authorization"
        || normalized.ends_with("_authorization")
        || is_sensitive_credential_key(&normalized)
        || normalized
            .strip_prefix("x_")
            .is_some_and(is_sensitive_credential_key)
}

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
            Some((k, _)) if is_sensitive_credential_key(k) => {
                format!("{k}={}", Config::REDACTED)
            }
            _ => param.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Whether `flag` is a sensitive CLI flag whose value must be redacted.
fn is_sensitive_arg_flag(flag: &str) -> bool {
    if !flag.starts_with('-') {
        return false;
    }
    let flag = flag.trim_start_matches('-');
    let normalized = normalized_credential_key(flag);
    SENSITIVE_ARG_FLAGS
        .iter()
        .any(|candidate| normalized_credential_key(candidate.trim_start_matches('-')) == normalized)
        || is_sensitive_credential_key(&normalized)
}

/// Redact the values of sensitive flags in an args vector, handling both
/// `--flag=VALUE` (redact the tail) and `--flag VALUE` (redact the next arg).
/// Over-redaction is safe for an audit dump; under-redaction is not.
fn redact_arg_secrets(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut redact_next = false;
    let mut redact_header_next = false;
    for arg in args {
        if redact_next {
            out.push(Config::REDACTED.to_string());
            redact_next = false;
            continue;
        }
        if redact_header_next {
            if arg
                .split_once(':')
                .is_some_and(|(name, _)| is_sensitive_header_name(name.trim()))
            {
                out.push(Config::REDACTED.to_string());
            } else {
                out.push(arg.clone());
            }
            redact_header_next = false;
            continue;
        }
        match arg.split_once('=') {
            Some((flag, _)) if is_sensitive_arg_flag(flag) => {
                out.push(format!("{flag}={}", Config::REDACTED));
            }
            Some((flag, value)) if matches!(flag, "-H" | "--header") => {
                if value
                    .split_once(':')
                    .is_some_and(|(name, _)| is_sensitive_header_name(name.trim()))
                {
                    out.push(format!("{flag}={}", Config::REDACTED));
                } else {
                    out.push(arg.clone());
                }
            }
            _ if arg.strip_prefix("-H").is_some_and(|value| {
                !value.is_empty()
                    && value
                        .split_once(':')
                        .is_some_and(|(name, _)| is_sensitive_header_name(name.trim()))
            }) =>
            {
                out.push(format!("-H{}", Config::REDACTED));
            }
            _ if ["-b", "-u"]
                .iter()
                .find_map(|flag| arg.strip_prefix(flag).map(|value| (*flag, value)))
                .is_some_and(|(_, value)| !value.is_empty()) =>
            {
                let flag = &arg[..2];
                out.push(format!("{flag}{}", Config::REDACTED));
            }
            _ if is_sensitive_arg_flag(arg) => {
                // `--flag VALUE`: keep the flag, redact the following value.
                out.push(arg.clone());
                redact_next = true;
            }
            _ if matches!(arg.as_str(), "-H" | "--header") => {
                out.push(arg.clone());
                redact_header_next = true;
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

/// Top-level config keys that grant **control-plane authority** — command
/// execution, the exec backend, or inference/data endpoints. A walked-up
/// project `.newt/config.toml` is attacker-reachable (a cloned repo can ship
/// one), so these keys are stripped from an untrusted project overlay before it
/// is merged: a hostile repo cannot silently run a command or redirect the
/// agent's endpoints via config alone. This is data, not logic — extend the
/// table, not the merge code (the three-Cs convention).
///
/// `mcp_servers` is deliberately absent: it has its own literal-only untrusted
/// gate ([`mark_project_mcp_untrusted`] + `McpTrust::Untrusted`), which keeps a
/// project's stdio services usable without ever interpolating `${cmd:…}` or
/// running a ref — a finer treatment than a blanket strip.
pub(crate) const CONTROL_PLANE_KEYS: &[&str] = &[
    "providers",       // `[[providers]]` subprocess plugins — arbitrary command execution
    "lifecycle",       // build / check / lint shell commands — arbitrary command execution
    "shell",           // the shell/exec backend selection (host vs confined)
    "backends",        // inference endpoints — every prompt + context is sent there (exfil)
    "default_backend", // selects the active backend (an attacker-pinned one, if present)
    "discovery",       // backend auto-discovery endpoints (exfil)
    "dgx",             // DGX endpoints + ssh (exfil / remote exec)
    "scratch",         // external scratch paths
    // `[network] owned_suffixes` is the operator's "these hosts are mine"
    // declaration (#1789). It grants no authority, but it decides which
    // endpoints get the patient seven-attempt retry policy instead of the
    // thrifty hosted one — so a repo could make newt hammer a billable
    // third-party endpoint seven times per failure by declaring its suffix
    // owned. Same class as `discovery`: a repo has no business telling the
    // operator which hosts they own.
    "network",
    // `[crews.*].test` / `loop_program` are shell verification commands run on
    // `newt crew` (config.rs Crew.test → WorktreeWorkspace test_cmd → sh -c),
    // and a `[loadouts.*]` with only a model passes validation — so a project
    // overlay could mint a command by declaring the sole crew (auto-selected).
    // Confined by `run_confined_build`, but still config-minted exec authority:
    // strip both so an untrusted overlay cannot introduce a crew/loadout at all.
    "crews",
    "loadouts",
    // `[tui.permissions]` is the SESSION AUTHORITY preset — `to_caveats()` turns
    // it into the caveats the turn runs under (config.rs mcp_probe_caveats /
    // caveats_for_session). A project overlay setting `preset = "full-access"` /
    // `extra_exec` / `net` would escalate an ordinary interactive turn to
    // `Caveats::top()`. A repo has no business setting the operator's permission
    // authority, so the whole `[tui]` table is stripped from an untrusted config
    // (convergence-audit finding: repo-controlled posture escalation).
    "tui",
];

/// Remove every [`CONTROL_PLANE_KEYS`] entry from an untrusted config table in
/// place, at the `toml::Value` layer — *before* `try_into::<Config>()`, so a
/// stripped key fails closed to the trusted base's value (or the built-in
/// default), never the attacker's. A no-op on a non-table value.
pub(crate) fn strip_control_plane(value: &mut toml::Value) {
    if let Some(table) = value.as_table_mut() {
        for key in CONTROL_PLANE_KEYS {
            table.remove(*key);
        }
    }
}

/// Merge an **untrusted** project overlay over the trusted base, stripping every
/// control-plane key from the overlay first ([`strip_control_plane`]). The
/// replacement for a raw [`merge_toml`] of a walked-up `.newt/config.toml`: the
/// repo can still pin benign, non-control-plane preferences (rules, context
/// tuning, `[merge]` strategy), but never executable/endpoint authority.
pub(crate) fn merge_project_overlay(
    base: &mut toml::Value,
    mut overlay: toml::Value,
    arrays: ArrayMergeStrategy,
) {
    strip_control_plane(&mut overlay);
    merge_toml(base, overlay, arrays);
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
