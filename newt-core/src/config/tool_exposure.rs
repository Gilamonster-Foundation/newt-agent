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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Explicit tool allow-list. When `Some`, only the named tools are shown to
    /// the model; every other tool is hidden (moved to [`HiddenTools`]) and
    /// remains authorized at dispatch. Applies on top of — and overrides —
    /// `profile`-driven selection: even Kernel tools absent from this list are
    /// hidden. An empty list hides everything.
    ///
    /// Set in `[tool_exposure]` as `allow_list = ["read_file", "run_command"]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_list: Option<Vec<String>>,

    /// When `true`, `write_file`, `edit_file`, and `delete_file` are hidden
    /// regardless of `profile` or `allow_list`. Combined with a role-profile
    /// `readonly = true` the authority and visibility cannot drift apart.
    /// Default `false`.
    #[serde(default)]
    pub read_only: bool,
}

impl Default for ToolExposureConfig {
    fn default() -> Self {
        Self {
            profile: ExposureProfile::default(),
            schema_budget_pct: default_schema_budget_pct(),
            max_initial_tools: default_max_initial_tools(),
            supports_dynamic_catalog: default_supports_dynamic_catalog(),
            allow_list: None,
            read_only: false,
        }
    }
}

/// The `pi` preset (#2932): pi.dev's default coding-agent surface, the four
/// tools a weak local model handles well. `allow_list = PI_PRESET_TOOLS`.
pub const PI_PRESET_TOOLS: &[&str] = &["read_file", "write_file", "edit_file", "run_command"];

impl ToolExposureConfig {
    /// Whether `allow_list` is exactly the `pi` preset (order-insensitive).
    pub fn is_pi_preset(&self) -> bool {
        self.allow_list.as_deref().is_some_and(|list| {
            list.len() == PI_PRESET_TOOLS.len()
                && PI_PRESET_TOOLS.iter().all(|t| list.iter().any(|n| n == t))
        })
    }

    /// The operator-facing summary of the tool surface for a status line:
    /// `None` when the model sees the whole catalog (nothing to show),
    /// otherwise `pi`, `read-only`, `pi · read-only`, `3 tools`, `no tools`.
    pub fn status_label(&self) -> Option<String> {
        let list = match &self.allow_list {
            None => None,
            Some(_) if self.is_pi_preset() => Some("pi".to_string()),
            Some(l) if l.is_empty() => Some("no tools".to_string()),
            Some(l) => Some(format!("{} tools", l.len())),
        };
        match (list, self.read_only) {
            (None, false) => None,
            (None, true) => Some("read-only".to_string()),
            (Some(l), false) => Some(l),
            (Some(l), true) => Some(format!("{l} · read-only")),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn with(list: Option<&[&str]>, read_only: bool) -> ToolExposureConfig {
        ToolExposureConfig {
            allow_list: list.map(|l| l.iter().map(|s| s.to_string()).collect()),
            read_only,
            ..Default::default()
        }
    }

    /// #2932: the status line shows the active tool profile, and shows
    /// nothing at all under the open default.
    #[test]
    fn status_label_names_the_preset_the_toggle_and_the_count() {
        assert_eq!(with(None, false).status_label(), None);
        assert_eq!(
            with(None, true).status_label().as_deref(),
            Some("read-only")
        );
        let pi: Vec<&str> = PI_PRESET_TOOLS.iter().rev().copied().collect();
        assert!(with(Some(&pi), false).is_pi_preset(), "order-insensitive");
        assert_eq!(with(Some(&pi), false).status_label().as_deref(), Some("pi"));
        assert_eq!(
            with(Some(&pi), true).status_label().as_deref(),
            Some("pi · read-only")
        );
        assert_eq!(
            with(Some(&["read_file", "find", "git"]), false)
                .status_label()
                .as_deref(),
            Some("3 tools")
        );
        assert_eq!(
            with(Some(&[]), false).status_label().as_deref(),
            Some("no tools")
        );
        assert!(!with(Some(&["read_file"]), false).is_pi_preset());
    }
}
