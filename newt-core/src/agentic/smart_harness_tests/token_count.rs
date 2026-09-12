//! Uses the existing real-store overflow fixture to distinguish admission
//! evidence from primary replies and ground committed count recovery.
use super::*;
use crate::backend_probe::TokenCount;

fn measured(tokens: usize) -> TokenCount {
    TokenCount {
        tokens,
        response_bytes: format!("{{ \"input_tokens\" : {tokens} }}\n").into_bytes(),
        method: "llama_chat_input_tokens",
    }
}

fn prepare(harness: &SmartHarness, model: &str) -> (ContentId, Vec<u8>) {
    let body = serde_json::json!({"model":model,"messages":[
        {"role":"user","content":"retained operator prompt"}
    ]});
    harness
        .record_messages(body["messages"].as_array().unwrap())
        .unwrap();
    let bytes = harness.request(&body, "openai").unwrap();
    (harness.state().unwrap().request.unwrap(), bytes)
}

/// A measured rejection has no primary response. Its committed count and
/// recovery decision must remain reachable after the only writer exits.
#[test]
fn committed_token_count_allows_recovery_without_fabricating_a_model_reply() {
    let directory = tempfile::tempdir().unwrap();
    let harness = durable_harness(directory.path());
    let (request, bytes) = prepare(&harness, "counted-model");
    let count = measured(1500);
    harness
        .record_token_count(Ok(&count), 1000)
        .expect("the measured candidate must commit a request-bound count");
    let count_head = harness.head().unwrap();
    assert!(harness.state().unwrap().reply.is_none());
    assert!(harness
        .state()
        .unwrap()
        .session
        .pending_replies()
        .is_empty());
    harness
        .context_exceeded(&signal(Some(900)))
        .expect("an admitted oversized count is sufficient recovery evidence");
    let recovery_head = harness.head().unwrap();
    assert!(harness.state().unwrap().reply.is_none());
    assert!(harness
        .state()
        .unwrap()
        .session
        .pending_replies()
        .is_empty());
    assert!(
        harness.context_exceeded(&signal(None)).is_err(),
        "a consumed admission cannot authorize another recovery"
    );
    drop(harness);

    let restored = Session::restore(directory.path(), recovery_head, "local-session").unwrap();
    assert_eq!(restored.replay(request).unwrap(), bytes);
    assert!(restored.pending_replies().is_empty());
    assert_eq!(
        restored.restored_messages().unwrap(),
        vec![serde_json::json!({
            "role":"user","content":"retained operator prompt"
        })]
    );
    let store = FrameStore::open(directory.path()).unwrap();
    let count_journal = inspect(&store, count_head);
    assert_eq!(
        count_journal.record["request_intervention"]["request"],
        request.to_string()
    );
    let count_event: ContentId = count_journal.record["request_intervention"]["event"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let event = inspect(&store, count_event);
    assert_eq!(event.record["origin"], "harness");
    assert_eq!(event.record["kind"], "intervention");
    let payload_id = event.record["payload"].as_str().unwrap().parse().unwrap();
    let payload: Value = serde_json::from_slice(&store.source(&payload_id).unwrap()).unwrap();
    assert_eq!(payload["tokens"], 1500);
    assert_eq!(payload["budget"], 1000);
    assert_eq!(payload["method"], count.method);
    assert_eq!(
        payload["response_body"].as_str().unwrap().as_bytes(),
        count.response_bytes
    );
    let recovery_journal = inspect(&store, recovery_head);
    assert_eq!(
        recovery_journal.parents,
        vec![count_head],
        "admission recovery must not insert a fabricated reply or model outcome"
    );
    let recovery_event: ContentId = recovery_journal.record["intervention"]["event"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(inspect(&store, recovery_event).parents, vec![count_event]);
}

#[test]
fn only_an_admitted_current_request_rejection_can_authorize_count_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let harness = durable_harness(directory.path());
    let count = measured(1500);
    let head = harness.head().unwrap();
    assert!(harness.record_token_count(Ok(&count), 1000).is_err());
    assert_eq!(harness.head().unwrap(), head);
    prepare(&harness, "first");
    harness
        .record_token_count(Ok(&measured(500)), 1000)
        .unwrap();
    let accepted_head = harness.head().unwrap();
    assert!(harness.context_exceeded(&signal(None)).is_err());
    assert_eq!(harness.head().unwrap(), accepted_head);

    let error = anyhow::anyhow!("token-count endpoint returned 401: unauthorized");
    harness.record_token_count(Err(&error), 1000).unwrap();
    let failure_head = harness.head().unwrap();
    assert!(harness.context_exceeded(&signal(None)).is_err());
    assert_eq!(harness.head().unwrap(), failure_head);

    harness.record_token_count(Ok(&count), 1000).unwrap();
    prepare(&harness, "different-template");
    let new_request_head = harness.head().unwrap();
    assert!(harness.context_exceeded(&signal(None)).is_err());
    assert_eq!(harness.head().unwrap(), new_request_head);
}

#[test]
fn committed_tokenizer_capacity_error_allows_recovery_without_a_model_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let harness = durable_harness(directory.path());
    prepare(&harness, "test");
    let error =
        anyhow::anyhow!("token-count endpoint returned 400: Context size has been exceeded");
    harness.record_token_count(Err(&error), 1000).unwrap();
    let count_head = harness.head().unwrap();
    harness.context_exceeded(&signal(None)).unwrap();
    let store = FrameStore::open(directory.path()).unwrap();
    assert_eq!(
        inspect(&store, harness.head().unwrap()).parents,
        vec![count_head]
    );
    assert!(harness
        .state()
        .unwrap()
        .session
        .pending_replies()
        .is_empty());
    assert!(harness.state().unwrap().reply.is_none());
}

/// Grounds recording failure in a blocked real checkpoint locator. The error
/// reports observed count/error evidence as well as the failed persistence.
#[test]
fn token_count_persistence_failure_surfaces_evidence_and_stops_further_requests() {
    for probe_failed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let harness = durable_harness(directory.path());
        prepare(&harness, "test");
        harness
            .record_token_count(Ok(&measured(1600)), 1000)
            .unwrap();
        let head = harness.head().unwrap();
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
        let count = measured(1500);
        let observed =
            anyhow::anyhow!("token-count endpoint returned 400: Context size has been exceeded");
        let observation = if probe_failed {
            Err(&observed)
        } else {
            Ok(&count)
        };
        let error = harness.record_token_count(observation, 1000).unwrap_err();
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("token-count evidence"), "{diagnostic}");
        assert!(
            diagnostic.contains("frame storage") || diagnostic.contains("run writer conflict"),
            "retain the failed persistence cause: {diagnostic}"
        );
        assert!(
            diagnostic.contains(if probe_failed {
                "Context size has been exceeded"
            } else {
                "1500"
            }),
            "{diagnostic}"
        );
        assert_eq!(harness.head().unwrap(), head);
        let recovery_error = harness.context_exceeded(&signal(None)).unwrap_err();
        assert!(
            recovery_error.to_string().contains("observed rejection"),
            "a failed replacement count must clear stale admission authority: {recovery_error}"
        );
        assert!(harness
            .request(
                &serde_json::json!({"messages":[{"role":"user","content":"task"}]}),
                "openai"
            )
            .is_err());
    }
}
