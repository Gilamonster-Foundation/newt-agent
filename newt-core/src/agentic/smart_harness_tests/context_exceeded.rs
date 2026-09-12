//! Grounds overflow accounting in the same durable graph used for restart.
use super::*;
use crate::agentic::observability::BehaviorSignal;
use agent_harness::forensics::{inspect_from_store, Inspection};
use agent_harness::store::FrameStore;

fn durable_harness(directory: &std::path::Path) -> SmartHarness {
    SmartHarness::new(
        Session::open(directory, Default::default()).unwrap(),
        Arc::new(|_| Box::pin(async { panic!("overflow never needs adjudication") })),
        Default::default(),
    )
    .unwrap()
}

fn inspect(store: &FrameStore, id: ContentId) -> Inspection {
    inspect_from_store(store, id, Default::default())
        .unwrap()
        .unwrap()
}

fn signal(projected_tokens: Option<usize>) -> BehaviorSignal {
    BehaviorSignal::ContextExceeded {
        round: 1,
        attempt: 1,
        estimated_tokens: 1500,
        projected_tokens,
    }
}

/// Grounds the in-memory overflow signal in a reopened filesystem journal:
/// the rejected request, error body, and corrected request remain replayable.
#[test]
fn context_exceeded_reprojection_survives_restore_with_rejected_request_ancestry() {
    for projected_tokens in [Some(1000), None] {
        let directory = tempfile::tempdir().unwrap();
        let harness = durable_harness(directory.path());
        let original = serde_json::json!({"messages":[
            {"role":"user","content":"retained operator prompt"},
            {"role":"assistant","content":"old material"}
        ]});
        harness
            .record_messages(original["messages"].as_array().unwrap())
            .unwrap();
        let rejected_bytes = harness.request(&original, "openai").unwrap();
        let rejected_id = harness.state().unwrap().request.unwrap();
        let error_body = br#"{"error":{"message":"Context size has been exceeded"}}"#;
        harness.observe(error_body).unwrap();
        let rejected_reply = harness.state().unwrap().reply.unwrap();
        let reply_head = harness.head().unwrap();

        let event = signal(projected_tokens);
        harness.context_exceeded(&event).unwrap();
        let intervention_head = harness.head().unwrap();
        assert!(harness
            .state()
            .unwrap()
            .session
            .pending_replies()
            .is_empty());
        assert!(harness.state().unwrap().reply.is_none());
        assert_eq!(
            harness
                .state()
                .unwrap()
                .session
                .restored_messages()
                .unwrap(),
            original["messages"].as_array().unwrap().clone(),
            "accounting does not fabricate tool output or add a conversation message"
        );

        let corrected = projected_tokens.map(|_| {
            let body = serde_json::json!({"messages":[
                {"role":"user","content":"retained operator prompt"}
            ]});
            let bytes = harness.request(&body, "openai").unwrap();
            let id = harness.state().unwrap().request.unwrap();
            (id, bytes)
        });
        let head = harness.head().unwrap();
        drop(harness);

        let restored = Session::restore(directory.path(), head, "local-session").unwrap();
        assert_eq!(restored.replay(rejected_id).unwrap(), rejected_bytes);
        if let Some((id, bytes)) = corrected {
            assert_eq!(restored.replay(id).unwrap(), bytes);
        }
        assert!(restored.pending_replies().is_empty());
        let store = FrameStore::open(directory.path()).unwrap();
        let rejected = inspect(&store, reply_head);
        assert_eq!(
            rejected.record["reply"]["request"],
            serde_json::to_value(rejected_id).unwrap()
        );
        let journal = inspect(&store, intervention_head);
        let intervention_id = journal.references[0].cid.parse().unwrap();
        let intervention = inspect(&store, intervention_id);
        assert_eq!(intervention.parents, vec![rejected_reply]);
        assert_eq!(intervention.record["origin"], "harness");
        assert_eq!(intervention.record["kind"], "intervention");
        let payload = intervention
            .references
            .iter()
            .find(|reference| reference.relation == "payload")
            .unwrap();
        let payload: Value =
            serde_json::from_slice(&store.source(&payload.cid.parse().unwrap()).unwrap()).unwrap();
        assert_eq!(
            payload["request_cid"],
            serde_json::to_value(rejected_id).unwrap()
        );
        assert_eq!(payload["signal"], serde_json::to_value(event).unwrap());
    }
}

#[test]
fn context_exceeded_without_an_observed_rejection_refuses_without_moving_head() {
    let directory = tempfile::tempdir().unwrap();
    let harness = durable_harness(directory.path());
    harness
        .request(
            &serde_json::json!({"messages":[{"role":"user","content":"task"}]}),
            "openai",
        )
        .unwrap();
    let head = harness.head().unwrap();
    let error = harness.context_exceeded(&signal(None)).unwrap_err();
    assert!(error.to_string().contains("observed rejection"));
    assert_eq!(harness.head().unwrap(), head);
}

/// Grounds persistence error propagation in an actual failed head publication.
#[test]
fn context_exceeded_checkpoint_failure_is_returned_and_prevents_another_request() {
    let directory = tempfile::tempdir().unwrap();
    let harness = durable_harness(directory.path());
    harness
        .request(
            &serde_json::json!({"messages":[{"role":"user","content":"task"}]}),
            "openai",
        )
        .unwrap();
    harness.observe(b"Context size has been exceeded").unwrap();
    std::fs::rename(
        directory.path().join("heads"),
        directory.path().join("retained-heads"),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("heads"),
        b"blocked checkpoint directory",
    )
    .unwrap();
    assert!(harness.context_exceeded(&signal(Some(1000))).is_err());
    assert!(harness
        .request(
            &serde_json::json!({"messages":[{"role":"user","content":"task"}]}),
            "openai"
        )
        .is_err());
}

#[path = "token_count.rs"]
mod token_count;
