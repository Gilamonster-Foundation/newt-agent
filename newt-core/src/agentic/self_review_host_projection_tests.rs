//! Existing dispatch-interface regressions for canonical step provenance.
//! These observe real plan_exec arguments, not production child review execution.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;

use super::{crew_tool::CrewRunner, plan_exec::run_plan};
use crate::{plan::Plan, Caveats};

#[derive(Default)]
struct Recorder(
    Mutex<Vec<Value>>,
    Mutex<Vec<(Option<crate::kit::CapturedTechniques>, Option<String>)>>,
);

#[async_trait]
impl CrewRunner for Recorder {
    async fn dispatch(
        &self,
        op: &str,
        args: &Value,
        _caveats: &Caveats,
        context: crate::agentic::CrewDispatchContext<'_>,
    ) -> Result<String, String> {
        assert_eq!(op, "crew");
        self.0.lock().unwrap().push(args.clone());
        self.1.lock().unwrap().push((
            context.techniques.cloned(),
            context.plan_step.map(str::to_owned),
        ));
        Ok("fixture dispatch; no branch artifact".into())
    }
}

/// #2449: labels alone cannot let the production child capture actual step
/// provenance. The canonical id must cross the same dispatch boundary.
#[tokio::test]
async fn self_review_host_selected_plan_forwards_actual_step_id() {
    let mut plan = Plan::from_toml_str("[[subtask]]\nid = \"canonical-review-step\"\ninstruction = \"Inspect existing artifact\"\ntechniques = [\"self_review\"]").unwrap();
    let runner = Recorder::default();
    assert!(run_plan(&mut plan, &Caveats::top(), &runner).await.complete);
    let calls = runner.0.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["techniques"], serde_json::json!(["self_review"]));
    assert_eq!(calls[0]["plan_step"], "canonical-review-step");
}

/// Unselected plans preserve the existing argument shape and child contract.
#[tokio::test]
async fn self_review_host_unselected_plan_retains_only_canonical_context() {
    let mut plan = Plan::from_toml_str(
        "[[subtask]]\nid = \"ordinary-step\"\ninstruction = \"Inspect existing artifact\"",
    )
    .unwrap();
    let runner = Recorder::default();
    assert!(run_plan(&mut plan, &Caveats::top(), &runner).await.complete);
    let calls = runner.0.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].get("techniques").is_none());
    assert_eq!(calls[0]["plan_step"], "ordinary-step");
    assert!(calls[0].get("review_subject").is_none());
}

/// Real plan executor projection of the captured parent, still not a claim
/// that LocalCrewRunner consumed it or executed the review phase.
#[tokio::test]
async fn self_review_host_inherited_policy_crosses_trusted_plan_dispatch() {
    let profile: crate::config::ProfileConfig =
        toml::from_str("techniques = [\"self_review\"]\n[self_review]\nmax_rounds = 1").unwrap();
    let pick = crate::config::ProfilePick {
        name: "parent".into(),
        via: crate::config::PickVia::Profile,
    };
    let captured = crate::kit::CapturedTechniques::capture(Some((&pick, &profile)), []).unwrap();
    let mut plan = Plan::from_toml_str(
        "[[subtask]]\nid = \"actual-inherited-step\"\ninstruction = \"Inspect artifact\"",
    )
    .unwrap();
    let runner = Recorder::default();
    let outcome = super::plan_exec::run_plan_with_reground(
        &mut plan,
        &Caveats::top(),
        &runner,
        &super::plan_exec::NoReground,
        Some(&captured),
    )
    .await;
    assert!(outcome.complete);
    assert!(runner.0.lock().unwrap()[0].get("techniques").is_none());
    let received = runner.1.lock().unwrap();
    assert_eq!(received[0].0.as_ref(), Some(&captured));
    assert_eq!(received[0].1.as_deref(), Some("actual-inherited-step"));
    assert_eq!(
        received[0]
            .0
            .as_ref()
            .unwrap()
            .self_review()
            .unwrap()
            .max_rounds,
        1
    );
    assert!(!received[0]
        .0
        .as_ref()
        .unwrap()
        .sources("self_review")
        .iter()
        .any(|source| matches!(source, crate::kit::TechniqueSource::PlanStep { .. })));
}
