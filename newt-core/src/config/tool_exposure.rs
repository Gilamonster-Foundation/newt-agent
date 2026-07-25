use serde::{Deserialize, Serialize};

/// The exposure profile selected under `[tool_exposure].profile`.
///
/// `Full` (the default) is the identity controller — the advertised tool set is
/// bit-for-bit unchanged. `Auto` engages budget-driven selection using the
/// model's LIVE usable budget (probed `safe_context` → send budget). `Minimal`
/// is the aggressive pocket-multitool tier: kernel + the sticky active set only,
/// plus whatever budget still allows.
///
/// Deliberately NOT keyed on the model name — the budget input is the live,
/// probed context, so a model that grows more capable (or a bigger `num_ctx`)
/// widens the surface automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureProfile {
    /// Identity — advertise the full authorized catalog (unchanged behaviour).
    #[default]
    Full,
    /// Budget-driven selection seeded by the live usable context.
    Auto,
    /// Kernel + sticky active set only, plus what the budget allows.
    Minimal,
}

/// `[tool_exposure]` — the progressive tool-schema controller (Pass 1).
///
/// Governs how much of the model's usable context the advertised tool schemas
/// may occupy. `None` (section absent) → [`ExposureProfile::Full`] defaults,
/// i.e. the pre-controller behaviour. See
/// `docs/design/tool-exposure-controller.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolExposureConfig {
    /// Which exposure policy to run. Default `full` (identity).
    pub profile: ExposureProfile,

    /// Percent of the live usable budget the tool schemas may occupy under
    /// `auto` / `minimal`. Default 15. Ignored under `full`.
    #[serde(default = "default_schema_budget_pct")]
    pub schema_budget_pct: u16,

    /// Hard cap on the number of exposed tools (0 = unlimited). A safety rail
    /// for when token estimates are wrong — the budget is the real governor.
    #[serde(default = "default_max_initial_tools")]
    pub max_initial_tools: usize,

    /// Whether the backend permits per-round catalog changes. Reserved for the
    /// per-round working-set pass; unused in Pass 1. Default true.
    #[serde(default = "default_supports_dynamic_catalog")]
    pub supports_dynamic_catalog: bool,
}

impl Default for ToolExposureConfig {
    fn default() -> Self {
        Self {
            profile: ExposureProfile::default(),
            schema_budget_pct: default_schema_budget_pct(),
            max_initial_tools: default_max_initial_tools(),
            supports_dynamic_catalog: default_supports_dynamic_catalog(),
        }
    }
}

pub(crate) fn default_schema_budget_pct() -> u16 {
    15
}

pub(crate) fn default_max_initial_tools() -> usize {
    0
}

pub(crate) fn default_supports_dynamic_catalog() -> bool {
    true
}
