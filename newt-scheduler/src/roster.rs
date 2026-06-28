//! roster.rs — compose a crew/team/panel **roster** from the live environment.
//!
//! The autonomous-composer layer (`/crew-roster`): given what's actually
//! reachable (the live models the [`BackendPool`] survey returns) plus capability
//! **priors** (heuristic now; empirical model-family profiles as the rig lands),
//! propose WHICH model fills WHICH role and whether to run as a **crew/team**
//! (division of labor) or a **panel** (decorrelated voices).
//!
//! The output is a [`RosterSpec`] the overseer **shows the human for approval** —
//! every pick carries a one-line rationale. The composer never runs anything; it
//! proposes. Picks are deterministic (stable sort + first-seen tie-breaks), so the
//! same environment always yields the same proposal.

use crate::{BackendPool, CrewConfig, TeamConfig};
use newt_core::Tier;

/// How a composed roster runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterMode {
    /// One crew: planner/navigator/triage divide the labor on one task.
    Crew,
    /// A team: a lead decomposes the goal, a crew runs each subtask.
    Team,
    /// A panel: N decorrelated voices on the same task (anti-groupthink).
    Panel,
}

/// A capability **prior** for one model — what it is good (or bad) at. Heuristic
/// until the rig (#80) supplies empirical family numbers; an explicit prior always
/// overrides the name-based guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPrior {
    pub model: String,
    /// Suitability as planner/lead, `0..=100`.
    pub planning: u8,
    /// Suitability as navigator (context curation), `0..=100`.
    pub navigation: u8,
    /// Known to confabulate under retry — avoided as the SOLE planner/lead.
    pub fabricates: bool,
}

/// A proposed composition plus the rationale behind each pick (for human review).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterSpec {
    pub mode: RosterMode,
    /// Lead / decomposer (set for [`RosterMode::Team`]).
    pub lead: Option<String>,
    pub planner: String,
    pub navigator: String,
    pub triage: String,
    /// The decorrelated voices (set for [`RosterMode::Panel`]).
    pub voices: Vec<String>,
    /// One line per decision, e.g. `"planner ← qwen3-coder:30b (largest; low fabrication risk)"`.
    pub rationale: Vec<String>,
}

impl RosterSpec {
    /// The crew this roster fields (planner/navigator/triage + a retry budget).
    #[must_use]
    pub fn to_crew(&self, max_attempts: u32) -> CrewConfig {
        CrewConfig {
            navigator_model: self.navigator.clone(),
            planner_model: self.planner.clone(),
            triage_model: self.triage.clone(),
            max_attempts,
            role_timeout: None,
        }
    }

    /// The team this roster fields: the lead decomposes, the crew runs each subtask.
    #[must_use]
    pub fn to_team(&self, lead_tier: Tier, max_attempts: u32, max_subtasks: usize) -> TeamConfig {
        TeamConfig {
            lead_model: self.lead.clone().unwrap_or_else(|| self.planner.clone()),
            lead_tier,
            crew: self.to_crew(max_attempts),
            max_subtasks,
        }
    }
}

/// The model "family" key for diversity — the leading alphabetic run of the name
/// (`qwen2.5-coder:3b` → `qwen`, `nemotron-3-nano:4b` → `nemotron`). Two models
/// of the same family share blind spots, so a panel wants distinct families.
fn family(model: &str) -> String {
    model
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Rough parameter size in billions, parsed from a `<n>b` tag (`…:32b` → 32,
/// `…:4b` → 4). `0` when absent — a conservative "small" default.
fn param_b(model: &str) -> u32 {
    let bytes = model.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if (c == b'b' || c == b'B') && i > 0 {
            let start = bytes[..i]
                .iter()
                .rposition(|d| !d.is_ascii_digit())
                .map_or(0, |p| p + 1);
            if start < i {
                if let Ok(n) = model[start..i].parse::<u32>() {
                    return n;
                }
            }
        }
    }
    0
}

/// The prior to use for `model`: an explicit one if supplied, else inferred from
/// the name (size → planning, "coder" → navigation, "nemotron" → fabricates — the
/// cross-family confabulation finding).
fn prior_for(model: &str, priors: &[ModelPrior]) -> ModelPrior {
    if let Some(p) = priors.iter().find(|p| p.model == model) {
        return p.clone();
    }
    let size = param_b(model);
    let planning = u8::try_from((30 + size * 2).min(98)).unwrap_or(98);
    let is_coder =
        model.contains("coder") || model.contains("codestral") || model.contains("deepseek");
    let navigation = if is_coder {
        (planning / 2 + 40).min(95)
    } else {
        planning / 2
    };
    ModelPrior {
        model: model.to_string(),
        planning,
        navigation,
        fabricates: model.contains("nemotron"),
    }
}

/// Compose a roster from the models the environment actually offers. Returns
/// `None` if nothing is available. The picks: **planner** = strongest planner that
/// does not fabricate (falling back to strongest if all do); **navigator** =
/// strongest navigator, preferring a model other than the planner; **triage** =
/// the smallest/cheapest model. For a panel, the top distinct **families**.
#[must_use]
pub fn compose_roster(
    available: &[String],
    priors: &[ModelPrior],
    mode: RosterMode,
) -> Option<RosterSpec> {
    if available.is_empty() {
        return None;
    }
    let mut models: Vec<ModelPrior> = available.iter().map(|m| prior_for(m, priors)).collect();
    // Stable, deterministic ordering before any tie-broken pick.
    models.sort_by(|a, b| a.model.cmp(&b.model));

    let mut rationale = Vec::new();

    // planner: highest planning, non-fabricators first.
    let planner = models
        .iter()
        .filter(|p| !p.fabricates)
        .max_by_key(|p| p.planning)
        .or_else(|| models.iter().max_by_key(|p| p.planning))
        .map(|p| p.model.clone())?;
    let planner_fab = models
        .iter()
        .find(|p| p.model == planner)
        .is_some_and(|p| p.fabricates);
    rationale.push(format!(
        "planner ← {planner} ({}{})",
        "strongest planner",
        if planner_fab {
            "; ⚠ fabrication-prone (no clean alternative)"
        } else {
            "; low fabrication risk"
        }
    ));

    // navigator: highest navigation, prefer a model other than the planner.
    let navigator = models
        .iter()
        .filter(|p| p.model != planner)
        .max_by_key(|p| p.navigation)
        .or_else(|| models.iter().max_by_key(|p| p.navigation))
        .map(|p| p.model.clone())
        .unwrap_or_else(|| planner.clone());
    rationale.push(format!(
        "navigator ← {navigator} (best context-curation score)"
    ));

    // triage: the smallest/cheapest model (light, diagnostic role).
    let triage = models
        .iter()
        .min_by_key(|p| param_b(&p.model))
        .map(|p| p.model.clone())
        .unwrap_or_else(|| planner.clone());
    rationale.push(format!(
        "triage ← {triage} (smallest/cheapest for diagnosis)"
    ));

    // panel voices: the strongest model from each distinct family (decorrelation).
    let mut voices: Vec<String> = Vec::new();
    if mode == RosterMode::Panel {
        let mut seen_families: Vec<String> = Vec::new();
        // strongest-first so each family is represented by its best model.
        let mut by_strength = models.clone();
        by_strength.sort_by(|a, b| b.planning.cmp(&a.planning).then(a.model.cmp(&b.model)));
        for p in &by_strength {
            let fam = family(&p.model);
            if !seen_families.contains(&fam) {
                seen_families.push(fam);
                voices.push(p.model.clone());
            }
        }
        rationale.push(format!(
            "panel ← {} voices across {} distinct families (anti-groupthink)",
            voices.len(),
            seen_families.len()
        ));
    }

    let lead = match mode {
        RosterMode::Team => {
            rationale.push(format!("lead ← {planner} (decomposes the goal)"));
            Some(planner.clone())
        }
        _ => None,
    };

    Some(RosterSpec {
        mode,
        lead,
        planner,
        navigator,
        triage,
        voices,
        rationale,
    })
}

/// Convenience: survey a live [`BackendPool`] and compose in one call.
#[must_use]
pub fn compose_from_pool(
    pool: &BackendPool,
    priors: &[ModelPrior],
    mode: RosterMode,
) -> Option<RosterSpec> {
    compose_roster(&pool.live_models(), priors, mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_b_parses_size_tags() {
        assert_eq!(param_b("qwen2.5-coder:32b"), 32);
        assert_eq!(param_b("nemotron-3-nano:4b"), 4);
        assert_eq!(param_b("deepseek-coder-v2:16b"), 16);
        assert_eq!(param_b("some-model"), 0);
    }

    #[test]
    fn family_strips_to_leading_alpha() {
        assert_eq!(family("qwen2.5-coder:3b"), "qwen");
        assert_eq!(family("nemotron-3-nano:4b"), "nemotron");
        assert_eq!(family("codestral:22b"), "codestral");
    }

    #[test]
    fn heuristic_picks_largest_coder_as_planner() {
        let models = vec![
            "qwen2.5-coder:3b".to_string(),
            "qwen3-coder:30b".to_string(),
            "qwen2.5-coder:7b".to_string(),
        ];
        let r = compose_roster(&models, &[], RosterMode::Crew).unwrap();
        assert_eq!(r.planner, "qwen3-coder:30b", "largest is the planner");
        assert_eq!(r.triage, "qwen2.5-coder:3b", "smallest is triage");
        assert_ne!(r.navigator, r.planner, "navigator differs from planner");
    }

    #[test]
    fn avoids_fabricator_as_planner_when_alternative_exists() {
        // nemotron is larger but fabrication-prone; a clean coder should plan.
        let models = vec![
            "nemotron-3:33b".to_string(),
            "qwen2.5-coder:14b".to_string(),
        ];
        let r = compose_roster(&models, &[], RosterMode::Crew).unwrap();
        assert_eq!(r.planner, "qwen2.5-coder:14b");
        assert!(r
            .rationale
            .iter()
            .any(|l| l.contains("low fabrication risk")));
    }

    #[test]
    fn explicit_prior_overrides_name_heuristic() {
        let models = vec!["small-but-smart:1b".to_string(), "big-dumb:70b".to_string()];
        let priors = vec![ModelPrior {
            model: "small-but-smart:1b".to_string(),
            planning: 99,
            navigation: 50,
            fabricates: false,
        }];
        let r = compose_roster(&models, &priors, RosterMode::Crew).unwrap();
        assert_eq!(
            r.planner, "small-but-smart:1b",
            "prior beats size heuristic"
        );
    }

    #[test]
    fn panel_picks_distinct_families() {
        let models = vec![
            "qwen2.5-coder:7b".to_string(),
            "qwen3-coder:30b".to_string(), // same family as above
            "codestral:22b".to_string(),
            "deepseek-coder-v2:16b".to_string(),
        ];
        let r = compose_roster(&models, &[], RosterMode::Panel).unwrap();
        let fams: Vec<String> = r.voices.iter().map(|m| family(m)).collect();
        let mut uniq = fams.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(
            fams.len(),
            uniq.len(),
            "one voice per family, no dup families"
        );
        assert!(
            r.voices.contains(&"qwen3-coder:30b".to_string()),
            "family's strongest represents it"
        );
    }

    #[test]
    fn team_sets_lead_and_converts() {
        let models = vec![
            "qwen3-coder:30b".to_string(),
            "qwen2.5-coder:3b".to_string(),
        ];
        let r = compose_roster(&models, &[], RosterMode::Team).unwrap();
        assert_eq!(r.lead.as_deref(), Some("qwen3-coder:30b"));
        let team = r.to_team(Tier::Complex, 2, 4);
        assert_eq!(team.lead_model, "qwen3-coder:30b");
        assert_eq!(team.crew.triage_model, "qwen2.5-coder:3b");
    }

    #[test]
    fn empty_environment_composes_nothing() {
        assert!(compose_roster(&[], &[], RosterMode::Crew).is_none());
    }
}
