//! Model operations share host composition admission; catalogs are bounded.
use agent_harness::{composition::*, Session, SessionConfig};
use serde_json::json;

#[test]
fn model_summarise_is_a_durable_refusal_not_parking() {
    let mut s = Session::new(SessionConfig::default()).unwrap();
    let r = s.record_request(json!({"messages":[{"role":"system","content":"rules"},{"role":"user","content":"old"},{"role":"user","content":"task"}]}), "openai").unwrap();
    let (id, c) = s
        .composition_catalog(r.id, Policy { max_bytes: 8192 })
        .unwrap();
    let p = s
        .record_composition_proposal(
            id,
            Actor {
                model: "test".into(),
                harness: "test".into(),
            },
            Proposal {
                expected_head: c.expected_head,
                changes: vec![Change {
                    occurrence: c.entries[1].event,
                    action: serde_json::from_value(json!("Summarise")).unwrap(),
                    reason: "shorter".into(),
                }],
                inverse: None,
            },
        )
        .unwrap();
    let receipt = s.decide_composition(p).unwrap();
    assert_eq!(
        serde_json::to_value(receipt.outcome).unwrap(),
        json!({"Refused":"Unsupported"})
    );
    assert_eq!(s.composition_bytes().unwrap(), r.bytes);
}

fn actor() -> Actor {
    Actor {
        model: "fixture".into(),
        harness: "test".into(),
    }
}
fn messages() -> Vec<serde_json::Value> {
    vec![
        json!({"role":"system","content":"rules"}),
        json!({"role":"user","content":"old"}),
        json!({"role":"user","content":"task"}),
    ]
}

#[test]
fn queued_proposal_survives_restart_and_binds_appended_context() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = SessionConfig::default();
    let mut s = Session::open(dir.path(), cfg.clone()).unwrap();
    let a = s
        .record_request(json!({"messages":messages()}), "openai")
        .unwrap();
    let c = s.refresh_composition_catalog(a.id, 8192).unwrap();
    let (_, page) = s.composition_page(c, 0, 3, 4096).unwrap();
    assert_eq!(page.cards.len(), 3);
    s.queue_composition(
        c,
        actor(),
        Proposal {
            expected_head: page.expected_head,
            changes: vec![Change {
                occurrence: page.cards[1].cid.parse().unwrap(),
                action: Action::Park,
                reason: "old detail".into(),
            }],
            inverse: None,
        },
    )
    .unwrap();
    let head = s.head();
    drop(s);
    let mut s = Session::restore(dir.path(), head, &cfg.authority).unwrap();
    assert!(s.has_pending_composition());
    let mut next = messages();
    next.push(json!({"role":"assistant","content":"new reply"}));
    s.record_messages(&next).unwrap();
    let r = s
        .record_request(json!({"model":"real","messages":next}), "openai")
        .unwrap();
    let decisions = s.resolve_queued_composition(r.id, 8192).unwrap();
    assert_eq!(decisions[0].outcome, Outcome::Accepted);
    assert!(!s.has_pending_composition());
    let sent = s.record_composed_request().unwrap();
    assert_eq!(sent.composition, Some(decisions[0].decision));
    let wire: serde_json::Value = serde_json::from_slice(&sent.bytes).unwrap();
    assert_eq!(wire["messages"].as_array().unwrap().len(), 3);
    assert_eq!(wire["messages"][2]["content"], "new reply");
    let c = s.refresh_composition_catalog(sent.id, 8192).unwrap();
    let (_, page) = s.composition_page(c, 0, 4, 4096).unwrap();
    assert!(page.cards[1].parked);
    let head = s.head();
    drop(s);
    let s = Session::restore(dir.path(), head, &cfg.authority).unwrap();
    assert_eq!(s.replay(sent.id).unwrap(), sent.bytes);
}

#[test]
fn pages_are_bounded_and_unoffered_occurrences_cannot_be_queued() {
    let mut s = Session::new(SessionConfig::default()).unwrap();
    let r = s
        .record_request(json!({"messages":messages()}), "openai")
        .unwrap();
    let (id, c) = s
        .composition_catalog(r.id, Policy { max_bytes: 8192 })
        .unwrap();
    assert!(s.composition_page(id, 0, 3, 1).is_err());
    let (_, page) = s.composition_page(id, 0, 1, 1024).unwrap();
    assert_eq!(page.next_offset, Some(1));
    assert!(s
        .queue_composition(
            id,
            actor(),
            Proposal {
                expected_head: None,
                changes: vec![Change {
                    occurrence: c.entries[1].event,
                    action: Action::Park,
                    reason: "not offered".into()
                }],
                inverse: None
            }
        )
        .is_err());
    assert_eq!(s.composition_bytes().unwrap(), r.bytes);
    assert!(!s.has_pending_composition());
}

#[test]
fn queued_catalog_refuses_a_new_operator_objective() {
    let mut s = Session::new(SessionConfig::default()).unwrap();
    let r = s
        .record_request(json!({"messages":messages()}), "openai")
        .unwrap();
    let (c, _) = s
        .composition_catalog(r.id, Policy { max_bytes: 8192 })
        .unwrap();
    let (_, p) = s.composition_page(c, 0, 3, 4096).unwrap();
    s.queue_composition(
        c,
        actor(),
        Proposal {
            expected_head: p.expected_head,
            changes: vec![Change {
                occurrence: p.cards[1].cid.parse().unwrap(),
                action: Action::Park,
                reason: "old".into(),
            }],
            inverse: None,
        },
    )
    .unwrap();
    let mut next = messages();
    next.push(json!({"role":"user","content":"different objective"}));
    s.record_messages(&next).unwrap();
    let r = s
        .record_request(json!({"messages":next}), "openai")
        .unwrap();
    let out = s.resolve_queued_composition(r.id, 8192).unwrap();
    assert_eq!(out[0].outcome, Outcome::Refused(Refusal::Stale));
    assert_eq!(s.composition_bytes().unwrap(), r.bytes);
    assert!(!s.has_pending_composition());
}

#[test]
fn catalog_pages_share_retrieval_budget_and_proposals_share_retry_budget() {
    let cfg = SessionConfig {
        max_dereferences: 1,
        max_retries: 0,
        ..Default::default()
    };
    let mut s = Session::new(cfg).unwrap();
    let r = s
        .record_request(json!({"messages":messages()}), "openai")
        .unwrap();
    let (c, _) = s
        .composition_catalog(r.id, Policy { max_bytes: 8192 })
        .unwrap();
    let (_, p) = s.composition_page(c, 0, 3, 4096).unwrap();
    assert!(matches!(
        s.composition_page(c, 0, 3, 4096),
        Err(agent_harness::Error::Budget(_))
    ));
    let q = Proposal {
        expected_head: p.expected_head,
        changes: vec![],
        inverse: None,
    };
    s.queue_composition(c, actor(), q.clone()).unwrap();
    assert_eq!(
        s.resolve_queued_composition(r.id, 8192).unwrap()[0].outcome,
        Outcome::Refused(Refusal::NoProgress)
    );
    assert!(matches!(
        s.queue_composition(c, actor(), q),
        Err(agent_harness::Error::Budget(_))
    ));
}

#[test]
fn identical_request_retry_keeps_its_composition_binding() {
    let mut s = Session::new(SessionConfig::default()).unwrap();
    let r = s
        .record_request(json!({"messages":messages()}), "openai")
        .unwrap();
    let (c, cat) = s
        .composition_catalog(r.id, Policy { max_bytes: 8192 })
        .unwrap();
    let p = s
        .record_composition_proposal(
            c,
            actor(),
            Proposal {
                expected_head: None,
                changes: vec![Change {
                    occurrence: cat.entries[1].event,
                    action: Action::Park,
                    reason: "old".into(),
                }],
                inverse: None,
            },
        )
        .unwrap();
    let d = s.decide_composition(p).unwrap();
    let sent = s.record_composed_request().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&sent.bytes).unwrap();
    s.record_messages(body["messages"].as_array().unwrap())
        .unwrap();
    let retry = s.record_request(body, "openai").unwrap();
    assert_eq!(retry.composition, Some(d.decision));
}
