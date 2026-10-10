//! Composition is a host primitive, not a model tool or an execution capability.
use agent_harness::{
    composition::*, projection::Projection, store::FrameStore, Session, SessionConfig,
};
use content_addressable::ContentId;
use serde_json::{json, Value};

fn messages() -> Vec<Value> {
    vec![
        json!({"role":"system","content":"policy"}),
        json!({"role":"user","content":"old detail"}),
        json!({"role":"user","content":"old detail"}),
        json!({"role":"user","content":"current task"}),
    ]
}
fn actor() -> Actor {
    Actor {
        model: "fixture".into(),
        harness: "host test".into(),
    }
}
fn offer(s: &mut Session, request: ContentId) -> (ContentId, Catalog) {
    s.composition_catalog(request, Policy { max_bytes: 8192 })
        .unwrap()
}
fn propose(
    s: &mut Session,
    catalog: ContentId,
    c: &Catalog,
    changes: Vec<Change>,
    inverse: Option<ContentId>,
) -> Receipt {
    let p = s
        .record_composition_proposal(
            catalog,
            actor(),
            Proposal {
                expected_head: c.expected_head,
                changes,
                inverse,
            },
        )
        .unwrap();
    s.decide_composition(p).unwrap()
}
fn park(id: ContentId) -> Change {
    Change {
        occurrence: id,
        action: Action::Park,
        reason: "unneeded in this view".into(),
    }
}
fn request_from_view(s: &mut Session) -> agent_harness::PreparedRequest {
    s.record_composed_request().unwrap()
}

/// Repeated text is selected by occurrence, not payload; inverse is exact and
/// dispatch/restart retain the accepted decision rather than merely a CID list.
#[test]
fn composition_inverse_replays_exact_bytes_and_repeated_occurrences() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::open(dir.path(), SessionConfig::default()).unwrap();
    let a = s
        .record_request(json!({"model":"fixture","messages":messages()}), "openai")
        .unwrap();
    let (id, c) = offer(&mut s, a.id);
    assert_ne!(c.entries[1].event, c.entries[2].event);
    assert_eq!(c.entries[1].source, c.entries[2].source);
    let b = propose(&mut s, id, &c, vec![park(c.entries[1].event)], None);
    assert_eq!(b.outcome, Outcome::Accepted);
    let sent_b = request_from_view(&mut s);
    assert_eq!(sent_b.composition, Some(b.decision));
    let actual: Projection = FrameStore::open(dir.path())
        .unwrap()
        .get(&sent_b.projection)
        .unwrap();
    assert_eq!(actual.entries[1].event, c.entries[2].event);
    let (id, c2) = offer(&mut s, sent_b.id);
    let inverse = propose(&mut s, id, &c2, vec![], Some(b.decision));
    assert_eq!(inverse.projection, a.projection);
    let sent_a = request_from_view(&mut s);
    assert_eq!(sent_a.bytes, a.bytes);
    assert_eq!(sent_a.composition, Some(inverse.decision));
    let head = s.head();
    drop(s);
    let s = Session::restore(dir.path(), head, &SessionConfig::default().authority).unwrap();
    assert_eq!(s.replay(sent_a.id).unwrap(), a.bytes);
    assert_eq!(s.replay(sent_b.id).unwrap(), sent_b.bytes);
    assert_eq!(s.composition_bytes().unwrap(), a.bytes);
}

/// Pin/budget/unknown/duplicate/no-progress refusals retain the raw proposal
/// and cause, leave the live projection unchanged, and survive restart.
#[test]
fn composition_refusals_are_durable_without_view_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::open(dir.path(), SessionConfig::default()).unwrap();
    let a = s
        .record_request(json!({"messages":messages()}), "openai")
        .unwrap();
    let (id, c) = offer(&mut s, a.id);
    let r = propose(&mut s, id, &c, vec![park(c.entries[3].event)], None);
    assert_eq!(r.outcome, Outcome::Refused(Refusal::RequiredInput));
    assert_eq!(s.composition_bytes().unwrap(), a.bytes);
    let (id, c) = s
        .composition_catalog(a.id, Policy { max_bytes: 2 })
        .unwrap();
    let r = propose(&mut s, id, &c, vec![park(c.entries[1].event)], None);
    assert_eq!(r.outcome, Outcome::Refused(Refusal::Capacity));
    assert_eq!(s.composition_bytes().unwrap(), a.bytes);
    let (id, c) = offer(&mut s, a.id);
    let unknown = ContentId::from_canonical_bytes(b"foreign occurrence");
    let r = propose(&mut s, id, &c, vec![park(unknown)], None);
    assert_eq!(r.outcome, Outcome::Refused(Refusal::InvalidChange));
    let (id, c) = offer(&mut s, a.id);
    let r = propose(
        &mut s,
        id,
        &c,
        vec![park(c.entries[1].event), park(c.entries[1].event)],
        None,
    );
    assert_eq!(r.outcome, Outcome::Refused(Refusal::InvalidChange));
    let (id, c) = offer(&mut s, a.id);
    let r = propose(&mut s, id, &c, vec![], None);
    assert_eq!(r.outcome, Outcome::Refused(Refusal::NoProgress));
    let head = s.head();
    drop(s);
    let s = Session::restore(dir.path(), head, &SessionConfig::default().authority).unwrap();
    assert_eq!(
        s.composition_decision(r.decision).unwrap().outcome,
        r.outcome
    );
    assert_eq!(s.composition_bytes().unwrap(), a.bytes);
}

/// A changed objective or pin revision invalidates an inverse, including ABA
/// changes whose eventual pin text is identical to the original.
#[test]
fn composition_inverse_refuses_revised_pins_and_new_objectives() {
    for new_objective in [false, true] {
        let mut s = Session::new(SessionConfig::default()).unwrap();
        let mut m = messages();
        let pins = [HostPin {
            class: PinClass::Objective,
            index: 3,
        }];
        s.register_semantic_pins("one", &mut m, &pins).unwrap();
        let a = s.record_request(json!({"messages":m}), "openai").unwrap();
        let (id, c) = offer(&mut s, a.id);
        let b = propose(&mut s, id, &c, vec![park(c.entries[1].event)], None);
        let mut m: Vec<Value> = serde_json::from_slice::<Value>(&s.composition_bytes().unwrap())
            .unwrap()["messages"]
            .as_array()
            .unwrap()
            .clone();
        let last = m.len() - 1;
        let mut changed = m.clone();
        if !new_objective {
            changed[last]["content"] = json!("revised objective");
        }
        let pins = [HostPin {
            class: PinClass::Objective,
            index: last,
        }];
        s.register_semantic_pins(
            if new_objective { "two" } else { "one" },
            &mut changed,
            &pins,
        )
        .unwrap();
        if !new_objective {
            s.register_semantic_pins("one", &mut m, &pins).unwrap();
        } else {
            m = changed;
        }
        let now = s.record_request(json!({"messages":m}), "openai").unwrap();
        let (id, c) = offer(&mut s, now.id);
        let r = propose(&mut s, id, &c, vec![], Some(b.decision));
        assert_eq!(r.outcome, Outcome::Refused(Refusal::Stale));
        assert_eq!(s.composition_bytes().unwrap(), now.bytes);
    }
}

/// Missing or substituted parked sources stop admission, dispatch and restore;
/// retained pointers are not allowed to conceal unavailable source material.
#[test]
fn composition_verifies_missing_and_tampered_source_closures() {
    for missing in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::open(dir.path(), SessionConfig::default()).unwrap();
        let a = s
            .record_request(json!({"messages":messages()}), "openai")
            .unwrap();
        let (id, c) = offer(&mut s, a.id);
        let p = s
            .record_composition_proposal(
                id,
                actor(),
                Proposal {
                    expected_head: c.expected_head,
                    changes: vec![park(c.entries[1].event)],
                    inverse: None,
                },
            )
            .unwrap();
        let source = dir.path().join(c.entries[1].source.to_string());
        if missing {
            std::fs::remove_file(source).unwrap();
        } else {
            std::fs::write(source, b"tampered").unwrap();
        }
        assert!(s.decide_composition(p).is_err());
        assert!(s.composition_bytes().is_err());
        let head = s.head();
        drop(s);
        assert!(Session::restore(dir.path(), head, &SessionConfig::default().authority).is_err());
    }
}

/// Inclusion restores a parked occurrence in original chronology; inverses
/// compose across successive decisions without replaying tools or rewriting heads.
#[test]
fn composition_include_and_composed_inverses_keep_order() {
    let mut s = Session::new(SessionConfig::default()).unwrap();
    let a = s
        .record_request(json!({"messages":messages()}), "openai")
        .unwrap();
    let (id, c) = offer(&mut s, a.id);
    let b = propose(&mut s, id, &c, vec![park(c.entries[1].event)], None);
    let (id, c2) = offer(&mut s, a.id);
    let c_receipt = propose(&mut s, id, &c2, vec![park(c.entries[2].event)], None);
    let (id, c3) = offer(&mut s, a.id);
    let undo_c = propose(&mut s, id, &c3, vec![], Some(c_receipt.decision));
    assert_eq!(undo_c.projection, b.projection);
    let (id, c4) = offer(&mut s, a.id);
    let undo_b = propose(&mut s, id, &c4, vec![], Some(b.decision));
    assert_eq!(undo_b.projection, a.projection);
    assert_eq!(s.composition_bytes().unwrap(), a.bytes);
    let (id, c) = offer(&mut s, a.id);
    propose(&mut s, id, &c, vec![park(c.entries[1].event)], None);
    let (id, c) = offer(&mut s, a.id);
    let include = Change {
        occurrence: c.entries[1].event,
        action: Action::Include,
        reason: "needed again".into(),
    };
    let r = propose(&mut s, id, &c, vec![include], None);
    assert_eq!(r.projection, a.projection);
    assert_eq!(s.composition_bytes().unwrap(), a.bytes);
}

/// A late source observation invalidates a proposal even when pins and text
/// happen to be unchanged; reasons cannot paper over a changed host context.
#[test]
fn composition_refuses_stale_context_and_policy_inverse() {
    let mut s = Session::new(SessionConfig::default()).unwrap();
    let a = s
        .record_request(json!({"messages":messages()}), "openai")
        .unwrap();
    let (id, c) = offer(&mut s, a.id);
    let pending = s
        .record_composition_proposal(
            id,
            actor(),
            Proposal {
                expected_head: c.expected_head,
                changes: vec![park(c.entries[1].event)],
                inverse: None,
            },
        )
        .unwrap();
    s.record_reply(a.id, b"new observation").unwrap();
    let r = s.decide_composition(pending).unwrap();
    assert_eq!(r.outcome, Outcome::Refused(Refusal::Stale));
    assert_eq!(s.composition_bytes().unwrap(), a.bytes);
    let (id, c) = offer(&mut s, a.id);
    let b = propose(&mut s, id, &c, vec![park(c.entries[1].event)], None);
    let (id, c) = s
        .composition_catalog(a.id, Policy { max_bytes: 8000 })
        .unwrap();
    let r = propose(&mut s, id, &c, vec![], Some(b.decision));
    assert_eq!(r.outcome, Outcome::Refused(Refusal::Stale));
    assert_eq!(r.projection, b.projection);
}

/// A missing parked source also invalidates replay of an otherwise intact B
/// request, because its accepted decision retains the complete A source closure.
#[test]
fn composition_replay_requires_parked_sources_and_exact_request_binding() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Session::open(dir.path(), SessionConfig::default()).unwrap();
    let a=s.record_request(json!({"messages":[{"role":"user","content":"unique old detail"},{"role":"user","content":"task"}]}),"openai").unwrap();
    let (id, c) = offer(&mut s, a.id);
    propose(&mut s, id, &c, vec![park(c.entries[0].event)], None);
    let b = request_from_view(&mut s);
    let before = s.head();
    let mut changed: Value = serde_json::from_slice(&b.bytes).unwrap();
    changed["extra"] = json!(true);
    assert!(s.record_request(changed, "openai").is_err());
    assert_eq!(
        s.head(),
        before,
        "refused wire must not publish a different projection"
    );
    let mut changed: Value = serde_json::from_slice(&b.bytes).unwrap();
    changed["messages"][0]["content"] = json!("different host context must be registered first");
    assert!(s.record_request(changed, "openai").is_err());
    assert_eq!(s.head(), before);
    assert_eq!(s.composition_bytes().unwrap(), b.bytes);
    std::fs::remove_file(dir.path().join(c.entries[0].source.to_string())).unwrap();
    assert!(s.replay(b.id).is_err());
    assert!(
        agent_harness::replay_from_store(&FrameStore::open(dir.path()).unwrap(), b.id).is_err()
    );
}

/// The inverse retains renderer, template, ordering and exact provider bytes,
/// including Anthropic's transformed messages and Responses' input field.
#[test]
fn composition_inverse_preserves_provider_renderers() {
    for wire in ["openai", "ollama", "responses", "anthropic"] {
        let mut s = Session::new(SessionConfig::default()).unwrap();
        let m = messages();
        let a = if wire == "anthropic" {
            s.record_rendered_request(json!({"model":"fixture","messages":[]}), wire, &m)
                .unwrap()
        } else {
            let field = if wire == "responses" {
                "input"
            } else {
                "messages"
            };
            let mut body = json!({"model":"fixture"});
            body[field] = json!(m);
            s.record_request(body, wire).unwrap()
        };
        let (id, c) = offer(&mut s, a.id);
        let b = propose(&mut s, id, &c, vec![park(c.entries[1].event)], None);
        let sent_b = s.record_composed_request().unwrap();
        assert_eq!(sent_b.composition, Some(b.decision));
        let (id, c) = offer(&mut s, sent_b.id);
        propose(&mut s, id, &c, vec![], Some(b.decision));
        let inverse = s.record_composed_request().unwrap();
        assert_eq!(inverse.bytes, a.bytes, "{wire}");
        assert_eq!(s.replay(inverse.id).unwrap(), a.bytes, "{wire}");
    }
}

/// Live results and their full call groups use the same selection validator;
/// a reason is never permission to split an exchange or drop system policy.
#[test]
fn composition_cannot_park_system_or_split_live_tool_exchanges() {
    let mut s = Session::new(SessionConfig::default()).unwrap();
    let a=s.record_request(json!({"messages":[
        {"role":"system","content":"policy"}, {"role":"user","content":"task"},
        {"role":"assistant","content":null,"tool_calls":[{"id":"call","type":"function","function":{"name":"read","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"call","content":"observed bytes"}
    ]}),"openai").unwrap();
    for index in [0, 2, 3] {
        let (id, c) = offer(&mut s, a.id);
        let r = propose(&mut s, id, &c, vec![park(c.entries[index].event)], None);
        assert_eq!(r.outcome, Outcome::Refused(Refusal::RequiredInput));
        assert_eq!(s.composition_bytes().unwrap(), a.bytes);
    }
}

/// A host catalog capacity refusal is a budget outcome, not corrupt history;
/// it must not poison the writer or replace the last recorded view.
#[test]
fn composition_catalog_capacity_does_not_abort_the_session() {
    let config = SessionConfig {
        max_catalog_entries: 1,
        ..Default::default()
    };
    let mut s = Session::new(config).unwrap();
    let a = s
        .record_request(json!({"messages":messages()}), "openai")
        .unwrap();
    let head = s.head();
    assert!(matches!(
        s.composition_catalog(a.id, Policy { max_bytes: 8192 }),
        Err(agent_harness::Error::Budget(_))
    ));
    assert_eq!(s.head(), head);
    assert_eq!(s.composition_bytes().unwrap(), a.bytes);
    assert_eq!(s.composition_head().unwrap(), None);
}
