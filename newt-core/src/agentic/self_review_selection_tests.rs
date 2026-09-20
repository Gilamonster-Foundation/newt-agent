//! #2449 selection/validation regressions using existing interfaces.
//! These exercise loading and the real plan projection, not a review model phase.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{crew_tool::CrewRunner, plan_exec::run_plan};
use crate::config::{Config, Loadout, PickVia, ProfileConfig};
use crate::plan::Plan;
use crate::role_profile::RoleProfile;
use crate::{Caveats, CaveatsExt};

fn profile(text: &str) -> ProfileConfig {
    toml::from_str(text).expect("profile fixture parses")
}

/// The three shipped techniques must retain their validation and default knobs.
#[test]
fn existing_techniques_and_presupposition_remain_validated() {
    let current = profile(r#"techniques = ["knowledge_base", "verify_gate", "retry"]"#);
    assert_eq!(current.validate(), Ok(()));
    assert_eq!(current.retry_knobs().max_retries, 2);
    assert!(profile(r#"techniques = ["retry"]"#)
        .validate()
        .unwrap_err()
        .contains("verify_gate"));
}

/// Unknown names must still fail rather than become an inert claimed technique.
#[test]
fn unknown_profile_technique_is_rejected() {
    let error = profile(r#"techniques = ["not_a_technique"]"#)
        .validate()
        .unwrap_err();
    assert!(error.contains("not_a_technique"));
}

/// #2449: independent self-review selection must not require a cognition level.
/// This is selection evidence only; the all-wire executor test remains required.
#[test]
fn self_review_profile_is_valid_with_cognition_explicitly_off() {
    let _off = crate::cognition::scoped_effective_cognition(None);
    let selected = profile(r#"techniques = ["self_review"]"#);
    assert_eq!(selected.validate(), Ok(()));
    assert!(selected.enables("self_review"));
    assert_eq!(crate::cognition::effective_cognition(), None);
}

/// #2449: an explicitly present empty knob table supplies the default of three.
/// This checks the loaded value, not a future turn-policy capture API.
#[test]
fn self_review_empty_knob_table_defaults_to_three() {
    let selected = profile("[self_review]\n");
    let value = serde_json::to_value(selected).unwrap();
    assert_eq!(value["self_review"]["max_rounds"], json!(3));
}

/// #2449: copied profile data must preserve explicit knobs independently of edits.
/// A profile clone is not evidence that the production worker captures this value.
#[test]
fn self_review_explicit_knob_survives_profile_clone() {
    let mut selected = profile("[self_review]\nmax_rounds = 2\n");
    let captured = selected.clone();
    selected = profile("[self_review]\nmax_rounds = 1\n");
    assert_eq!(
        serde_json::to_value(captured).unwrap()["self_review"]["max_rounds"],
        json!(2)
    );
    assert_eq!(
        serde_json::to_value(selected).unwrap()["self_review"]["max_rounds"],
        json!(1)
    );
}

fn knob_error(value: &str) -> String {
    // No selected technique: a generic unknown-self_review error cannot make
    // malformed knob validation pass accidentally.
    let input = format!("[self_review]\nmax_rounds = {value}\n");
    match toml::from_str::<ProfileConfig>(&input) {
        Err(error) => error.to_string(),
        Ok(loaded) => loaded
            .validate()
            .expect_err("invalid knob must fail loading/validation"),
    }
}

/// #2449: negative counts cannot be silently dropped as an unknown table.
#[test]
fn self_review_negative_max_rounds_is_rejected() {
    assert!(knob_error("-1").contains("max_rounds"));
}

/// #2449: a string count cannot be silently dropped as an unknown table.
#[test]
fn self_review_nonnumeric_max_rounds_is_rejected() {
    assert!(knob_error(r#""three""#).contains("max_rounds"));
}

/// #2449: named loadouts resolve the same independently selected profile.
#[test]
fn self_review_named_profile_survives_loadout_resolution() {
    let mut cfg = Config::default();
    cfg.profiles
        .insert("review".into(), profile(r#"techniques = ["self_review"]"#));
    let loadout: Loadout = toml::from_str(r#"profile = "review""#).unwrap();
    assert_eq!(loadout.validate(&cfg), Ok(()));
    let pick = cfg
        .pick_active_profile(Some("review"), None, None)
        .unwrap()
        .unwrap();
    assert_eq!(pick.via, PickVia::Profile);
    assert!(cfg
        .resolve_profile(&pick.name)
        .unwrap()
        .enables("self_review"));
}

/// #2449: persona/role unknown selectors must be rejected at the load boundary.
#[test]
fn unknown_persona_technique_is_rejected() {
    let error = RoleProfile::parse("+++\ntechniques = [\"not_a_technique\"]\n+++\nReview.")
        .expect_err("unknown technique must not disappear");
    assert!(error.to_string().contains("not_a_technique"));
}

/// #2449: the shared persona/role loader must retain an explicit selector.
#[test]
fn self_review_persona_selection_roundtrips() {
    let role = RoleProfile::parse("+++\ntechniques = [\"self_review\"]\n+++\nReview.").unwrap();
    assert!(role.to_markdown().unwrap().contains("\"self_review\""));
    assert_eq!(role.cognition, None);
}

/// #2449: metadata that selects executable review is not a prompt-only persona.
#[test]
fn self_review_selector_makes_persona_role_bound() {
    let role = RoleProfile::parse("+++\ntechniques = [\"self_review\"]\n+++\nReview.").unwrap();
    assert!(role.is_role_bound());
}

/// A plain persona remains valid and does not acquire technique metadata.
#[test]
fn prompt_only_persona_remains_unbound() {
    let role = RoleProfile::parse("Review the supplied diff.").unwrap();
    assert!(!role.is_role_bound());
    assert_eq!(role.cognition, None);
}

fn plan_text(selector: &str) -> String {
    format!(
        "[[subtask]]\nid = \"review\"\ninstruction = \"Inspect supplied artifact\"\n{selector}\n"
    )
}

/// #2449: canonical plan loading must recognize the selector and reject its
/// unknown value, not merely reject every techniques field as unknown schema.
#[test]
fn unknown_plan_technique_names_the_invalid_selection() {
    let error = Plan::from_toml_str(&plan_text("techniques = [\"not_a_technique\"]"))
        .expect_err("unknown technique must fail load");
    // Display includes the original TOML source; inspect only the diagnostic
    // so an unknown-field error cannot pass by echoing the invalid name.
    assert!(error.message().contains("not_a_technique"));
}

#[derive(Default)]
struct CaptureRunner(Mutex<Vec<(String, Value, Caveats)>>);

#[async_trait]
impl CrewRunner for CaptureRunner {
    async fn dispatch(
        &self,
        op: &str,
        args: &Value,
        caveats: &Caveats,
        _context: crate::agentic::CrewDispatchContext<'_>,
    ) -> Result<String, String> {
        self.0
            .lock()
            .unwrap()
            .push((op.to_owned(), args.clone(), caveats.clone()));
        Ok("fixture dispatch completed; no branch artifact".into())
    }
}

/// #2449: exercise canonical Subtask -> CrewTask -> run_plan -> CrewRunner.
/// The stub observes real dispatch arguments; it does not perform model review.
/// Before selector support, this fails at loading, not at the later assertion.
#[tokio::test]
async fn self_review_plan_selector_reaches_real_crew_dispatch() {
    let mut plan = Plan::from_toml_str(&plan_text("techniques = [\"self_review\"]")).unwrap();
    let runner = CaptureRunner::default();
    let result = run_plan(&mut plan, &Caveats::top(), &runner).await;
    assert!(result.complete);
    assert!(result.consolidated.is_none());
    let seen = runner.0.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "crew");
    assert_eq!(seen[0].1["techniques"], json!(["self_review"]));
    assert!(!seen[0].2.permits_exec("git"));
    assert!(!seen[0].2.permits_fs_write("artifact.txt"));
}

/// Existing plan dispatch without techniques remains valid and default-deny.
#[tokio::test]
async fn unselected_plan_dispatch_stays_unchanged() {
    let mut plan = Plan::from_toml_str(&plan_text("")).unwrap();
    let runner = CaptureRunner::default();
    let result = run_plan(&mut plan, &Caveats::top(), &runner).await;
    assert!(result.complete);
    let seen = runner.0.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].1.get("techniques").is_none());
    assert!(!seen[0].2.permits_exec("git"));
}
