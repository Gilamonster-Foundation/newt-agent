use agent_frame::Span;
use agent_harness::{
    projection::{Entry, Projection},
    store::FrameStore,
    Error, Session,
};
use content_addressable::{ContentAddressable, ContentId};
use serde_json::json;

fn assert_projection_boundary(messages: &[serde_json::Value]) {
    let size = serde_json::to_vec(messages).unwrap().len();
    let mut session = Session::new(Default::default()).unwrap();
    assert!(matches!(
        session.project(messages, size - 1),
        Err(Error::Budget(_))
    ));
    let mut session = Session::new(Default::default()).unwrap();
    assert_eq!(session.project(messages, size).unwrap(), messages);
}

#[test]
fn deterministic_projection_counts_array_delimiters() {
    assert_projection_boundary(&[]);
    assert_projection_boundary(&[json!({"role":"user","content":"x"})]);
}

#[test]
fn deterministic_projection_counts_separators_between_pinned_messages() {
    assert_projection_boundary(&[
        json!({"role":"system","content":"rules"}),
        json!({"role":"developer","content":"constraints"}),
        json!({"role":"user","content":"task"}),
    ]);
}

fn fixture() -> (FrameStore, Projection) {
    let mut store = FrameStore::memory();
    let mut entries = Vec::new();
    for (role, content) in [
        ("system", "rules"),
        ("user", "task"),
        ("assistant", "reply"),
    ] {
        let bytes = serde_json::to_vec(&json!({"role": role, "content": content})).unwrap();
        let source = store.put_source(&bytes).unwrap();
        entries.push(Entry {
            event: ContentId::from_canonical_bytes(role.as_bytes()),
            source,
            role: role.into(),
            span: Span::new(0, bytes.len() as u64),
        });
    }
    let template = store
        .put_source(br#"{"model":"test","messages":[],"tools":[]}"#)
        .unwrap();
    let projection = Projection {
        schema: 1,
        renderer: "json-messages-v1".into(),
        field: "messages".into(),
        template,
        entries,
    };
    (store, projection)
}

#[test]
fn cold_render_preserves_order_roles_and_all_request_inputs() {
    let (store, projection) = fixture();
    let bytes = projection.render(&store).unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["model"], "test");
    assert_eq!(body["messages"][1]["content"], "task");
    let mut reordered = projection.clone();
    reordered.entries.swap(1, 2);
    assert_ne!(
        projection.content_id().unwrap(),
        reordered.content_id().unwrap()
    );
    assert_ne!(bytes, reordered.render(&store).unwrap());
    let mut changed = projection.clone();
    changed.entries[1].role = "assistant".into();
    assert!(changed.render(&store).is_err());
    let mut changed = projection.clone();
    changed.entries[1].span.end -= 1;
    assert!(changed.render(&store).is_err());
    let mut changed = projection.clone();
    changed.renderer = "unavailable-renderer".into();
    assert!(changed.render(&store).is_err());
}

#[test]
fn missing_or_redacted_inputs_refuse_byte_perfect_replay() {
    let (_, projection) = fixture();
    assert!(projection.render(&FrameStore::memory()).is_err());
}

#[test]
fn anthropic_renderer_keeps_original_entries_when_wire_messages_coalesce() {
    let (store, mut projection) = fixture();
    projection.renderer = "anthropic-messages-v1".into();
    let body: serde_json::Value =
        serde_json::from_slice(&projection.render(&store).unwrap()).unwrap();
    assert_eq!(body["system"], "rules");
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(body["messages"][0]["content"][0]["text"], "task");
    assert_eq!(projection.entries[0].role, "system");
    assert_eq!(projection.entries.len(), 3);
}

#[test]
fn anthropic_renderer_refuses_source_shapes_it_cannot_preserve() {
    let (mut store, mut projection) = fixture();
    projection.renderer = "anthropic-messages-v1".into();
    let bytes = serde_json::to_vec(
        &json!({"role":"user","content":[{"type":"text","text":"protected operator material"}]}),
    )
    .unwrap();
    projection.entries[1].source = store.put_source(&bytes).unwrap();
    projection.entries[1].span = Span::new(0, bytes.len() as u64);
    assert!(projection.render(&store).is_err());
}
