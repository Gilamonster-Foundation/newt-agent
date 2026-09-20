//! Actual authored-plan parsing and pruning controls; no inference or dispatch.
use super::*;
use serde_json::json;

fn authored(review: serde_json::Value) -> String {
    json!({"subtasks":[
        {"id":"edit","instruction":"Implement fix"},
        {"id":"review","instruction":"Review existing work","deps":["edit"],
         "techniques":["self_review"],"review":review}
    ]})
    .to_string()
}

#[test]
fn self_review_authored_explicit_subject_and_selection_survive_parsing() {
    let plan = parse_authored_plan(&authored(json!({"kind":"existing_diff"}))).unwrap();
    let wire = serde_json::to_value(plan).unwrap();
    assert_eq!(
        wire["subtask"][1]["review"],
        json!({"kind":"existing_diff"})
    );
    assert_eq!(wire["subtask"][1]["techniques"], json!(["self_review"]));
}

#[test]
fn self_review_authored_explicit_review_leaf_survives_actual_prune() {
    let mut plan = parse_authored_plan(&authored(json!({"kind":"existing_diff"}))).unwrap();
    prune_non_actionable_subtasks_in(&mut plan, &effective_markers(None));
    assert!(
        plan.subtask("review").is_some(),
        "explicit review is actionable without edits"
    );
    assert_eq!(plan.subtask("review").unwrap().deps, ["edit"]);
}

#[test]
fn self_review_authored_invalid_review_cannot_be_silently_discarded() {
    for review in [
        json!({"kind":"unknown"}),
        json!({"kind":"artifacts","paths":[]}),
        json!({"kind":"existing_diff","max_rounds":999}),
    ] {
        assert!(parse_authored_plan(&authored(review)).is_none());
    }
}
