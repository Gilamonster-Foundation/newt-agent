//! The tool-exposure controller (Pass 1) — the fourth pipeline stage.
//!
//! The catalog pipeline is four ordered stages:
//!
//! ```text
//! known      = registry.all()                      // every dispatchable name
//! present    = filter_presence(known, session)     // Gate / injected capability
//! authorized = filter_authority(present, persona, disposition, caveats)
//! exposed    = exposure_policy.select(authorized, budget, active)   // THIS module
//! ```
//!
//! `present` is [`super::catalog::merged_tool_definitions`] (gate/presence),
//! `authorized` is [`super::catalog::filter_advertised_tools`] (persona) then
//! [`super::catalog::filter_tools_for_disposition`] (disposition). This module
//! is the final `exposed` stage: it decides which *authorized* tools are worth a
//! schema slot given the model's LIVE usable budget.
//!
//! **Exposure is never authorization.** Dispatch still checks the authorized set
//! (`tool_allowed` / `persona_tool_allowed`); hiding a schema to save tokens
//! never changes what the model may RUN, only what it is SHOWN. A model that
//! calls a real, authorized-but-unexposed tool is not hallucinating — see the
//! `ToolReach::KnownHidden` recovery reserved in `docs/design/tool-exposure-controller.md`.
//!
//! The primary control signal is the model's live usable budget (probed
//! `safe_context` → send budget), NOT its name. When no live budget signal
//! exists, the controller does not clip — no signal means no starvation.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::config::ExposureProfile;
use crate::tokens::TokenEstimation;

use super::catalog::BASE_TOOL_NAMES;

/// Maximum function tools accepted by the OpenAI-compatible function-calling
/// contract. This is a provider wire limit, not an authorization or operator
/// exposure preference: a `Full` profile may authorize more tools than can fit
/// in one request. Omitted tools remain authorized at dispatch and listable by
/// `tool_search`; this projection does not activate their schemas.
pub(crate) const OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS: usize = 128;

/// How a tool earns a schema slot, independent of whether it is *available*
/// (gate) or *authorized* (persona/disposition/caveats). Pure data — the class
/// for each tool lives in [`EXPOSURE_CLASSES`], guarded against drift by
/// [`tests::every_known_tool_is_classified`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExposureClass {
    /// Session-critical loop control + fundamental workspace verbs. Never
    /// evicted for budget; always exposed when authorized + present.
    Kernel,
    /// Loaded when task intent / recent use suggests it. Evictable under budget.
    ByIntent,
    /// Surfaced when the backing artifact/context exists (event-gating is a
    /// later pass; for now these evict early under budget pressure).
    RecoveryOnly,
    /// Only after explicit discovery / a `KnownHidden` retry (MCP + deferred
    /// families). Not exposed until promoted.
    OnDemand,
}

/// The exposure class of every non-base built-in tool, by name. Base tools
/// (`BASE_TOOL_NAMES`) are [`ExposureClass::Kernel`] by convention; unknown /
/// MCP (`server__tool`) names are [`ExposureClass::OnDemand`]. This is the ONE
/// place a tool's class is declared — [`tests::every_known_tool_is_classified`]
/// asserts it covers exactly the known catalog so a new tool cannot ship
/// unclassified.
const EXPOSURE_CLASSES: &[(&str, ExposureClass)] = &[
    // Kernel: discovery must always be present so a budget-clipped model can
    // still find what was hidden.
    ("tool_search", ExposureClass::Kernel),
    // Recovery affordances — only meaningful when their artifact/context exists.
    ("resume_context", ExposureClass::RecoveryOnly),
    ("prompt_read", ExposureClass::RecoveryOnly),
    ("artifact_read", ExposureClass::RecoveryOnly),
    ("get_context_remaining", ExposureClass::RecoveryOnly),
    ("request_user_input", ExposureClass::RecoveryOnly),
    ("render_report", ExposureClass::RecoveryOnly),
    // ByIntent: loaded when the task points at them.
    ("lifecycle", ExposureClass::ByIntent),
    ("save_note", ExposureClass::ByIntent),
    ("recall", ExposureClass::ByIntent),
    ("memory_fetch", ExposureClass::ByIntent),
    ("git", ExposureClass::ByIntent),
    ("compose_roster", ExposureClass::ByIntent),
    ("crew", ExposureClass::ByIntent),
    ("state_set", ExposureClass::ByIntent),
    ("state_get", ExposureClass::ByIntent),
    ("state_clear", ExposureClass::ByIntent),
    ("code_search", ExposureClass::ByIntent),
    // #1387 Code Navigator — inspection/navigation intent.
    ("where_is", ExposureClass::ByIntent),
    ("goto_definition", ExposureClass::ByIntent),
    ("text_search", ExposureClass::ByIntent),
    ("find_references", ExposureClass::ByIntent),
    ("find_tests", ExposureClass::ByIntent),
    ("find_callers", ExposureClass::ByIntent),
    ("find_callees", ExposureClass::ByIntent),
    ("find_implementations", ExposureClass::ByIntent),
    ("find_hierarchy", ExposureClass::ByIntent),
    ("inspect_type", ExposureClass::ByIntent),
    ("impact", ExposureClass::ByIntent),
    ("experience_record", ExposureClass::ByIntent),
    ("experience_recall", ExposureClass::ByIntent),
    ("update_plan", ExposureClass::ByIntent),
    ("plan_get", ExposureClass::ByIntent),
    ("enter_plan_mode", ExposureClass::ByIntent),
    ("exit_plan_mode", ExposureClass::ByIntent),
    ("select_operating_mode", ExposureClass::ByIntent),
];

/// The exposure class of a tool by name. Base tools are `Kernel`; a name found
/// in [`EXPOSURE_CLASSES`] takes its declared class; everything else (MCP
/// `server__tool`, genuinely unknown) is `OnDemand`.
#[must_use]
pub fn classify(name: &str) -> ExposureClass {
    if BASE_TOOL_NAMES.contains(&name) {
        return ExposureClass::Kernel;
    }
    EXPOSURE_CLASSES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| *c)
        .unwrap_or(ExposureClass::OnDemand)
}

/// The order bands fill remaining budget after Kernel + active. Lower index =
/// kept first. Kernel is mandatory (not in this list); active tools are kept
/// regardless of class before any band fill.
const FILL_ORDER: &[ExposureClass] = &[
    ExposureClass::ByIntent,
    ExposureClass::RecoveryOnly,
    ExposureClass::OnDemand,
];

/// A `Copy` snapshot of the resolved `[tool_exposure]` policy carried on
/// `ChatCtx`. Default is [`ExposureProfile::Full`] — the identity controller
/// (bit-for-bit unchanged advertised set).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExposureSettings {
    pub profile: ExposureProfile,
    /// Percent of the live usable budget spent on tool schemas (`auto`).
    pub schema_budget_pct: u16,
    /// Hard cap on exposed tool count (0 = unlimited). A safety rail, not the
    /// governor — the budget is.
    pub max_initial_tools: usize,
}

impl Default for ExposureSettings {
    fn default() -> Self {
        Self {
            profile: ExposureProfile::Full,
            schema_budget_pct: 15,
            max_initial_tools: 0,
        }
    }
}

impl From<crate::config::ToolExposureConfig> for ExposureSettings {
    fn from(c: crate::config::ToolExposureConfig) -> Self {
        Self {
            profile: c.profile,
            schema_budget_pct: c.schema_budget_pct,
            max_initial_tools: c.max_initial_tools,
        }
    }
}

/// What the controller decided this turn — for metrics / `KnownHidden` coaching.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExposurePlan {
    /// Names kept on the wire, in the catalog's original order.
    pub exposed: Vec<String>,
    /// Authorized names dropped from the wire (still dispatchable).
    pub hidden: Vec<String>,
    /// Estimated tokens of the exposed schema set.
    pub exposed_tokens: usize,
    /// The schema-token budget applied (`None` = no live signal / identity).
    pub budget_tokens: Option<usize>,
}

/// The name of a catalog entry (`{"function":{"name":…}}`), or `None`.
fn entry_name(def: &Value) -> Option<&str> {
    def.get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
}

/// Compute the schema-token budget from the live usable budget and profile.
/// `Full` → `None` (identity). `Auto`/`Minimal` scale `live_budget` by the
/// configured percent; `None` live budget under `Auto` yields `None` (do not
/// clip without a signal), while `Minimal` yields `Some(0)` (kernel+active only).
fn budget_tokens(settings: &ExposureSettings, live_budget_tokens: Option<usize>) -> Option<usize> {
    match settings.profile {
        ExposureProfile::Full => None,
        ExposureProfile::Auto => {
            live_budget_tokens.map(|b| b.saturating_mul(settings.schema_budget_pct as usize) / 100)
        }
        ExposureProfile::Minimal => Some(
            live_budget_tokens
                .map(|b| b.saturating_mul(settings.schema_budget_pct as usize) / 100)
                .unwrap_or(0),
        ),
    }
}

/// Plan which authorized tools to expose given the live budget and the sticky
/// `active` working set. Pure over the catalog `Value`; the actual filtering is
/// [`select_exposed`]. `Full` (or a non-array catalog) exposes everything.
#[must_use]
pub fn plan_exposure(
    defs: &Value,
    settings: &ExposureSettings,
    live_budget_tokens: Option<usize>,
    active: &BTreeSet<String>,
    est: TokenEstimation,
) -> ExposurePlan {
    let entries = defs.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let all_names: Vec<String> = entries
        .iter()
        .filter_map(|d| entry_name(d).map(str::to_owned))
        .collect();

    // Identity: Full profile, non-array catalog, or no budget under Auto.
    let Some(budget) = budget_tokens(settings, live_budget_tokens) else {
        let exposed_tokens = crate::agentic::trim::estimate_value_tokens(defs, est);
        return ExposurePlan {
            exposed: all_names,
            hidden: Vec::new(),
            exposed_tokens,
            budget_tokens: None,
        };
    };

    let mut kept: BTreeSet<String> = BTreeSet::new();
    let mut kept_tokens = 0usize;

    // Mandatory floor: Kernel + sticky active + any nameless (malformed) entry.
    // These are exempt from both the budget and `max_initial_tools`.
    for def in entries {
        let Some(name) = entry_name(def) else {
            continue;
        };
        let mandatory = classify(name) == ExposureClass::Kernel || active.contains(name);
        if mandatory && kept.insert(name.to_owned()) {
            kept_tokens += crate::agentic::trim::estimate_value_tokens(def, est);
        }
    }

    // Budget-order fill. Kernel + active are already in `kept` (and stay even
    // when they exceed the cap). The cap, when non-zero, is a total-count rail
    // on the final exposed set — additional fill stops once `kept.len()` hits it.
    let cap = settings.max_initial_tools;
    for band in FILL_ORDER {
        for def in entries {
            let Some(name) = entry_name(def) else {
                continue;
            };
            if kept.contains(name) || classify(name) != *band {
                continue;
            }
            if cap != 0 && kept.len() >= cap {
                break;
            }
            let cost = crate::agentic::trim::estimate_value_tokens(def, est);
            if kept_tokens.saturating_add(cost) > budget {
                continue;
            }
            kept.insert(name.to_owned());
            kept_tokens += cost;
        }
    }

    let mut exposed = Vec::new();
    let mut hidden = Vec::new();
    for name in all_names {
        if kept.contains(&name) {
            exposed.push(name);
        } else {
            hidden.push(name);
        }
    }
    ExposurePlan {
        exposed,
        hidden,
        exposed_tokens: kept_tokens,
        budget_tokens: Some(budget),
    }
}

/// Apply the exposure policy to an authorized catalog, returning the exposed
/// subset in original order. `Full` (or a non-array catalog) is identity. This
/// is the single seam the three agentic loops call after disposition filtering
/// and before the token estimate, so what is counted and what is sent agree.
#[must_use]
pub fn select_exposed(
    defs: Value,
    settings: &ExposureSettings,
    live_budget_tokens: Option<usize>,
    active: &BTreeSet<String>,
    est: TokenEstimation,
) -> Value {
    if settings.profile == ExposureProfile::Full {
        return defs;
    }
    let Value::Array(arr) = defs else {
        return defs;
    };
    let plan = plan_exposure(
        &Value::Array(arr.clone()),
        settings,
        live_budget_tokens,
        active,
        est,
    );
    let keep: BTreeSet<&str> = plan.exposed.iter().map(String::as_str).collect();
    Value::Array(
        arr.into_iter()
            .filter(|def| match entry_name(def) {
                Some(name) => keep.contains(name),
                None => true,
            })
            .collect(),
    )
}

/// Project an authorized/exposed catalog onto the OpenAI-compatible wire's
/// 128-function envelope.
///
/// Provider shape is the final pipeline constraint after exposure. Kernel
/// tools are selected first regardless of their current array position, then
/// the ordinary exposure bands fill the remaining slots. The returned array
/// preserves original catalog order so the projection is deterministic and
/// does not churn prompt prefixes. Authorization is unchanged: tools omitted
/// here remain governed by the dispatch boundary and listable through
/// `tool_search`, without implying that this projection activates them.
#[must_use]
pub(crate) fn select_openai_compatible_tools(defs: Value) -> Value {
    let Value::Array(arr) = defs else {
        return defs;
    };
    if arr.len() <= OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS {
        return Value::Array(arr);
    }

    let kernel_count = arr
        .iter()
        .filter_map(entry_name)
        .filter(|name| classify(name) == ExposureClass::Kernel)
        .count();
    debug_assert!(
        kernel_count <= OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS,
        "kernel tool catalog exceeds the OpenAI-compatible wire envelope"
    );

    let mut keep = vec![false; arr.len()];
    let mut kept = 0usize;
    for class in [
        ExposureClass::Kernel,
        ExposureClass::ByIntent,
        ExposureClass::RecoveryOnly,
        ExposureClass::OnDemand,
    ] {
        for (index, def) in arr.iter().enumerate() {
            if kept == OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS {
                break;
            }
            if entry_name(def).is_some_and(|name| classify(name) == class) {
                keep[index] = true;
                kept += 1;
            }
        }
    }

    Value::Array(
        arr.into_iter()
            .zip(keep)
            .filter_map(|(def, keep)| keep.then_some(def))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::super::catalog::ALL_TOOL_NAMES;
    use super::*;
    use serde_json::json;

    fn tool(name: &str, desc: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": desc,
                "parameters": {"type": "object", "properties": {}}
            }
        })
    }

    fn est() -> TokenEstimation {
        TokenEstimation::default()
    }

    #[test]
    fn every_known_tool_is_classified() {
        // Anti-drift: every dispatchable built-in must resolve to a class, and
        // no `EXPOSURE_CLASSES` row may name a tool that no longer exists.
        for name in ALL_TOOL_NAMES.iter() {
            let _ = classify(name); // never panics; asserts total coverage below
        }
        for (name, _) in EXPOSURE_CLASSES {
            assert!(
                ALL_TOOL_NAMES.contains(name),
                "EXPOSURE_CLASSES names `{name}` which is not a known tool"
            );
        }
        // Every non-base known tool must be explicitly listed (not defaulted to
        // OnDemand by accident) so a new tool ships with a deliberate class.
        for name in ALL_TOOL_NAMES.iter() {
            if BASE_TOOL_NAMES.contains(name) {
                continue;
            }
            assert!(
                EXPOSURE_CLASSES.iter().any(|(n, _)| n == name),
                "tool `{name}` has no explicit exposure class — add it to EXPOSURE_CLASSES"
            );
        }
    }

    #[test]
    fn base_tools_and_unknown_classify_by_convention() {
        assert_eq!(classify("read_file"), ExposureClass::Kernel);
        assert_eq!(classify("run_command"), ExposureClass::Kernel);
        assert_eq!(classify("tool_search"), ExposureClass::Kernel);
        assert_eq!(classify("git"), ExposureClass::ByIntent);
        assert_eq!(classify("resume_context"), ExposureClass::RecoveryOnly);
        assert_eq!(classify("github__create_issue"), ExposureClass::OnDemand);
        assert_eq!(classify("totally_made_up"), ExposureClass::OnDemand);
    }

    #[test]
    fn full_profile_is_identity() {
        let defs = json!([
            tool("read_file", "x"),
            tool("git", "y"),
            tool("impact", "z")
        ]);
        let out = select_exposed(
            defs.clone(),
            &ExposureSettings::default(),
            Some(100),
            &BTreeSet::new(),
            est(),
        );
        assert_eq!(out, defs, "Full must be bit-for-bit identity");
    }

    #[test]
    fn openai_wire_cap_keeps_kernel_before_optional_mcp_tools() {
        let mut defs = vec![tool("optional_server__tool_000", "remote")];
        defs.extend(
            (1..=OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS)
                .map(|i| tool(&format!("optional_server__tool_{i:03}"), "remote")),
        );
        // Put kernel tools at the end to prove selection is by exposure law,
        // not an accidental `truncate(128)` over today's catalog order.
        defs.push(tool("run_command", "kernel"));
        defs.push(tool("tool_search", "kernel"));

        let out = select_openai_compatible_tools(Value::Array(defs));
        let names = out
            .as_array()
            .unwrap()
            .iter()
            .filter_map(entry_name)
            .collect::<Vec<_>>();

        assert_eq!(names.len(), OPENAI_COMPATIBLE_MAX_FUNCTION_TOOLS);
        assert!(names.contains(&"run_command"));
        assert!(names.contains(&"tool_search"));
    }

    #[test]
    fn auto_without_live_budget_is_identity() {
        let defs = json!([tool("read_file", "x"), tool("git", "y")]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Auto,
            ..Default::default()
        };
        let out = select_exposed(defs.clone(), &settings, None, &BTreeSet::new(), est());
        assert_eq!(out, defs, "no live signal => no clipping");
    }

    #[test]
    fn auto_keeps_kernel_even_when_budget_is_tiny() {
        let defs = json!([
            tool("read_file", "kernel"),
            tool("tool_search", "kernel"),
            tool("git", "byintent"),
            tool("impact", "byintent"),
        ]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Auto,
            schema_budget_pct: 1,
            max_initial_tools: 0,
        };
        // Budget = 1% of 10 = 0 tokens: only Kernel survives.
        let plan = plan_exposure(&defs, &settings, Some(10), &BTreeSet::new(), est());
        assert!(plan.exposed.contains(&"read_file".to_string()));
        assert!(plan.exposed.contains(&"tool_search".to_string()));
        assert!(plan.hidden.contains(&"git".to_string()));
        assert!(plan.hidden.contains(&"impact".to_string()));
    }

    #[test]
    fn active_tools_are_sticky_across_budget() {
        let defs = json!([tool("read_file", "kernel"), tool("git", "byintent")]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Minimal,
            schema_budget_pct: 1,
            max_initial_tools: 0,
        };
        let mut active = BTreeSet::new();
        active.insert("git".to_string());
        let plan = plan_exposure(&defs, &settings, Some(0), &active, est());
        assert!(
            plan.exposed.contains(&"git".to_string()),
            "a recently-used tool must not vanish on a budget shift"
        );
    }

    #[test]
    fn auto_fills_by_budget_and_preserves_order() {
        // Big budget: everything fits; order preserved.
        let defs = json!([
            tool("read_file", "k"),
            tool("git", "b"),
            tool("impact", "b"),
        ]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Auto,
            schema_budget_pct: 100,
            max_initial_tools: 0,
        };
        let out = select_exposed(
            defs.clone(),
            &settings,
            Some(100_000),
            &BTreeSet::new(),
            est(),
        );
        assert_eq!(out, defs);
    }

    #[test]
    fn reduces_a_realistic_catalog_to_a_pocket_multitool() {
        // The proof a live 3.4k model cares about: over the REAL merged catalog
        // with every capability on, a tight budget keeps kernel + fits some
        // ByIntent, drops the rest, and the exposed set costs strictly less than
        // the full catalog — while nothing kernel is ever dropped.
        let full = super::super::catalog::merged_tool_definitions(
            &crate::agentic::NoMcp,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
        );
        let full_tokens = crate::agentic::trim::estimate_value_tokens(&full, est());
        let settings = ExposureSettings {
            profile: ExposureProfile::Auto,
            schema_budget_pct: 15,
            max_initial_tools: 0,
        };
        // A small model: ~4k usable tokens → ~600-token schema budget.
        let plan = plan_exposure(&full, &settings, Some(4_000), &BTreeSet::new(), est());
        assert!(
            plan.exposed_tokens < full_tokens,
            "exposed schema ({}) must cost less than the full catalog ({full_tokens})",
            plan.exposed_tokens
        );
        // NOTE: exposed_tokens is NOT asserted <= budget here — Kernel (which
        // includes the large base `run_command` schema) is exempt from the
        // budget by design. `auto_keeps_kernel_even_when_budget_is_tiny` covers
        // the budget-clip of the evictable (ByIntent) band deterministically.
        assert!(
            plan.exposed.iter().any(|n| n == "tool_search"),
            "discovery must survive so the model can find what was hidden"
        );
        assert!(
            !plan.hidden.is_empty(),
            "a tight budget over the full catalog must hide something"
        );
        // Every kernel tool present in the catalog must be exposed.
        for def in full.as_array().unwrap() {
            if let Some(name) = entry_name(def) {
                if classify(name) == ExposureClass::Kernel {
                    assert!(
                        plan.exposed.iter().any(|n| n == name),
                        "kernel tool `{name}` must never be evicted"
                    );
                }
            }
        }
    }

    #[test]
    fn max_initial_tools_caps_additional_fill() {
        let defs = json!([
            tool("read_file", "k"),
            tool("git", "b"),
            tool("impact", "b"),
            tool("code_search", "b"),
        ]);
        let settings = ExposureSettings {
            profile: ExposureProfile::Auto,
            schema_budget_pct: 100,
            max_initial_tools: 2,
        };
        let plan = plan_exposure(&defs, &settings, Some(100_000), &BTreeSet::new(), est());
        // Kernel read_file is mandatory; cap of 2 total limits ByIntent fill.
        assert!(plan.exposed.contains(&"read_file".to_string()));
        assert!(plan.exposed.len() <= 2);
    }
}
