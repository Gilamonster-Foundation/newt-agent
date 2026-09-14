use agent_harness::{
    forensics::{inspect_from_store, Inspection, InspectionLimits},
    store::FrameStore,
    Session,
};
use content_addressable::ContentId;
use serde_json::json;

fn inspect(store: &FrameStore, id: ContentId) -> Inspection {
    inspect_from_store(store, id, InspectionLimits::default())
        .unwrap()
        .unwrap()
}

#[test]
fn request_intervention_refuses_unknown_and_foreign_requests_without_moving_head() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path(), Default::default()).unwrap();
    let mut foreign = Session::open(dir.path(), Default::default()).unwrap();
    let foreign_request = foreign
        .record_request(
            json!({"model":"foreign","messages":[{"role":"user","content":"other run"}]}),
            "openai",
        )
        .unwrap();
    let head = session.head();
    let checkpoint = session.checkpoint_path().unwrap();
    let before = std::fs::read(&checkpoint).unwrap();
    for request in [session.run_id(), foreign_request.id] {
        assert!(session
            .record_request_intervention(request, b"token count")
            .is_err());
        assert_eq!(session.head(), head);
        assert_eq!(std::fs::read(&checkpoint).unwrap(), before);
        assert!(session.pending_replies().is_empty());
    }
    session.ensure_writer().unwrap();
}

/// Grounds request-bound count admission in real committed files and a fresh
/// restore, including a candidate that never reaches a model transport.
#[test]
fn rejected_candidate_count_survives_restore_without_fabricating_a_reply() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path(), Default::default()).unwrap();
    let messages = json!([{"role":"user","content":"keep this operator prompt"}]);
    session
        .record_messages(messages.as_array().unwrap())
        .unwrap();
    let previous = session
        .record_request(json!({"model":"previous","messages":messages}), "openai")
        .unwrap();
    let pending = session
        .record_reply(previous.id, b"an earlier unadjudicated observation")
        .unwrap();
    let request = session
        .record_request(json!({"model":"test","messages":messages}), "openai")
        .unwrap();
    let payload =
        br#"{"kind":"token_count","prompt_tokens":1500,"budget":1000,"decision":"reproject"}"#;
    let last_message = session.last_message();
    let event = session
        .record_request_intervention(request.id, payload)
        .expect("a prepared request must admit its preflight count before dispatch");
    assert_eq!(session.pending_replies(), vec![pending]);
    assert_eq!(session.last_message(), last_message);
    assert_eq!(session.replay(request.id).unwrap(), request.bytes);
    let head = session.head();
    drop(session);

    let restored = Session::restore(dir.path(), head, "local-session").unwrap();
    assert_eq!(restored.pending_replies(), vec![pending]);
    assert_eq!(restored.replay(request.id).unwrap(), request.bytes);
    assert_eq!(
        restored.restored_messages().unwrap(),
        messages.as_array().unwrap().clone()
    );
    let store = FrameStore::open(dir.path()).unwrap();
    let journal = inspect(&store, head);
    assert_eq!(
        journal.record["request_intervention"]["request"],
        request.id.to_string()
    );
    assert_eq!(
        journal.record["request_intervention"]["event"],
        event.to_string()
    );
    for (relation, id) in [("request", request.id), ("event", event)] {
        assert!(journal
            .references
            .iter()
            .any(|reference| reference.relation == relation && reference.cid == id.to_string()));
    }
    let recorded_event = inspect(&store, event);
    assert_eq!(recorded_event.record["origin"], "harness");
    assert_eq!(recorded_event.record["kind"], "intervention");
    assert_eq!(recorded_event.record["depth"], 1);
    assert_eq!(recorded_event.record["sources"], json!([]));
    let payload_id = recorded_event.record["payload"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(store.source(&payload_id).unwrap(), payload.to_vec());
    let projection = inspect(&store, request.projection);
    let expected_parents: std::collections::BTreeSet<ContentId> = projection.record["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["event"].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(
        recorded_event
            .parents
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        expected_parents
    );
}

#[test]
fn count_binding_distinguishes_request_templates_with_the_same_message_parents() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(dir.path(), Default::default()).unwrap();
    let messages = json!([{"role":"user","content":"the same messages"}]);
    let first = session
        .record_request(json!({"model":"count-a","messages":messages}), "openai")
        .unwrap();
    let first_event = session
        .record_request_intervention(first.id, b"{\"prompt_tokens\":120}")
        .expect("the first count must bind its exact prepared request");
    let first_head = session.head();
    let second = session
        .record_request(json!({"model":"count-b","messages":messages}), "openai")
        .unwrap();
    let second_event = session
        .record_request_intervention(second.id, b"{\"prompt_tokens\":125}")
        .unwrap();
    let second_head = session.head();
    let repeated_event = session
        .record_request_intervention(second.id, b"{\"prompt_tokens\":125}")
        .unwrap();
    assert_ne!(first.id, second.id);
    assert_ne!(first.bytes, second.bytes);
    assert_ne!(
        repeated_event, second_event,
        "each preflight is a fresh occurrence"
    );
    assert!(session.pending_replies().is_empty());
    let store = FrameStore::open(dir.path()).unwrap();
    assert_eq!(
        inspect(&store, first_event).parents,
        inspect(&store, second_event).parents
    );
    for (head, request, event) in [
        (first_head, first.id, first_event),
        (second_head, second.id, second_event),
        (session.head(), second.id, repeated_event),
    ] {
        let journal = inspect(&store, head);
        assert_eq!(
            journal.record["request_intervention"]["request"],
            request.to_string()
        );
        assert_eq!(
            journal.record["request_intervention"]["event"],
            event.to_string()
        );
    }
    assert_eq!(session.replay(first.id).unwrap(), first.bytes);
    assert_eq!(session.replay(second.id).unwrap(), second.bytes);
}
