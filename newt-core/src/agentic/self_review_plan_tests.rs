//! Canonical explicit read-only plan shape and actual plan-executor projection.
//! These do not claim LocalCrewRunner has executed or validated a review.
use crate::{plan::Plan, Caveats};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Mutex;

#[derive(Default)]
struct Recorder(Mutex<Vec<Value>>);
#[async_trait]
impl super::CrewRunner for Recorder {
    async fn dispatch(
        &self,
        op: &str,
        args: &Value,
        _caveats: &Caveats,
        context: super::CrewDispatchContext<'_>,
    ) -> Result<String, String> {
        assert_eq!(op, "crew");
        assert_eq!(context.plan_step, Some("review-step"));
        self.0.lock().unwrap().push(args.clone());
        Ok("test projection result; no execution evidence".into())
    }
}

#[test]
fn self_review_plan_existing_diff_roundtrips_without_literal_selector() {
    // A parent's captured selection may satisfy the final union at dispatch.
    let plan = Plan::from_toml_str("[[subtask]]\nid = 'review-step'\ninstruction = 'Review existing work'\n[subtask.review]\nkind = 'existing_diff'").unwrap();
    let wire = serde_json::to_value(&plan).unwrap();
    assert_eq!(
        wire["subtask"][0]["review"],
        json!({"kind":"existing_diff"})
    );
    assert_eq!(
        Plan::from_toml_str(&plan.to_toml_string().unwrap()).unwrap(),
        plan
    );
}

#[tokio::test]
async fn self_review_plan_artifacts_cross_existing_dispatch_without_write_scope() {
    let mut plan = Plan::from_toml_str("[[subtask]]\nid = 'review-step'\ninstruction = 'Review supplied artifact'\ntechniques = ['self_review']\n[subtask.review]\nkind = 'artifacts'\npaths = ['notes.md']").unwrap();
    let recorder = Recorder::default();
    let result = super::plan_exec::run_plan(&mut plan, &Caveats::top(), &recorder).await;
    assert!(result.complete);
    let calls = recorder.0.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["review"],
        json!({"kind":"artifacts","paths":["notes.md"]})
    );
    assert!(
        calls[0].get("scope").is_none(),
        "read subject is not a write lane"
    );
}

#[test]
fn self_review_plan_invalid_subject_shapes_reject_at_canonical_load() {
    for review in [
        json!({"kind":"unknown"}),
        json!({"kind":"artifacts","paths":[]}),
        json!({"kind":"artifacts","paths":[""]}),
        json!({"kind":"existing_diff","max_rounds":99}),
    ] {
        let input =
            json!({"subtask":[{"id":"review-step","instruction":"Review","review":review}]});
        assert!(serde_json::from_value::<Plan>(input).is_err());
    }
}

#[test]
fn self_review_plan_ordinary_shape_keeps_review_absent() {
    let plan =
        Plan::from_toml_str("[[subtask]]\nid = 'edit-step'\ninstruction = 'Implement change'")
            .unwrap();
    let wire = serde_json::to_value(plan).unwrap();
    assert!(wire["subtask"][0].get("review").is_none());
}
