//! Causal provenance and admission are the same at mint and decode boundaries.

use std::collections::{BTreeMap, BTreeSet};

use agent_frame::{
    Event, EventBody, EventKind, EventOrigin, Op, RawEvent, ReplyVerdict, RootEvent, RootKind,
    Span, Unit,
};
use content_addressable::{ContentAddressable, ContentId, MerkleNode, RawContentId};

type Events = BTreeMap<ContentId, Event>;

#[test]
fn every_event_kind_preserves_typed_cids_through_canonical_decode() {
    let source = body(EventKind::Observation, EventOrigin::Model, &[], 0);
    let unit = Unit::seal(Op::Elide, b"material", Span::new(0, 8), source.root).unwrap();
    for kind in [
        EventKind::Observation,
        EventKind::Intervention,
        EventKind::Verdict {
            verdict: ReplyVerdict::Question,
        },
        EventKind::Retrieval,
        EventKind::Elision {
            unit: unit.id().unwrap(),
        },
    ] {
        let raw = MerkleNode::new(
            EventBody {
                kind,
                ..source.clone()
            },
            [source.root],
        );
        let bytes = raw.canonical_form().unwrap();
        let decoded: RawEvent =
            content_addressable::canonical::from_canonical_dagcbor_checked(&bytes).unwrap();
        assert_eq!(decoded, raw);
        assert_eq!(decoded.canonical_form().unwrap(), bytes);
    }
}

fn body(kind: EventKind, origin: EventOrigin, sources: &[ContentId], depth: u32) -> EventBody {
    EventBody {
        root: RootEvent::new(RootKind::OperatorPrompt, b"task", 0)
            .id()
            .unwrap(),
        origin,
        kind,
        payload: RawContentId::from_content(b"material"),
        seq: 0,
        sources: sources.iter().copied().collect(),
        depth,
    }
}

fn append(events: &mut Events, body: EventBody, parents: &[ContentId]) -> ContentId {
    let event = Event::new(body, parents.iter().copied(), |id| events.get(id).cloned()).unwrap();
    let id = event.id().unwrap();
    events.insert(id, event);
    id
}

fn reply(events: &mut Events) -> ContentId {
    append(
        events,
        body(EventKind::Observation, EventOrigin::Model, &[], 0),
        &[],
    )
}

fn intervention(events: &mut Events, source: ContentId) -> ContentId {
    append(
        events,
        body(EventKind::Intervention, EventOrigin::Harness, &[source], 1),
        &[source],
    )
}

fn assert_refused(body: EventBody, parents: &[ContentId], events: &Events) {
    assert!(Event::new(body.clone(), parents.iter().copied(), |id| {
        events.get(id).cloned()
    })
    .is_err());
    let raw = MerkleNode::new(body, parents.iter().copied());
    let decoded: RawEvent = serde_json::from_slice(&serde_json::to_vec(&raw).unwrap()).unwrap();
    assert!(Event::admit(decoded, |id| events.get(id).cloned()).is_err());
}

#[test]
fn observations_roundtrip_without_turning_antecedents_into_derivation_sources() {
    let mut events = Events::new();
    let first = reply(&mut events);
    let nudge = intervention(&mut events, first);
    let next = append(
        &mut events,
        body(EventKind::Observation, EventOrigin::Model, &[], 0),
        &[nudge],
    );
    let observation = &events[&next];
    assert_eq!(observation.depth(), 0);
    assert_eq!(observation.parents(), &BTreeSet::from([nudge]));
    assert!(observation.body().sources.is_empty());
    let raw: RawEvent = serde_json::from_slice(&serde_json::to_vec(observation).unwrap()).unwrap();
    assert_eq!(
        Event::admit(raw, |id| events.get(id).cloned()).unwrap(),
        *observation
    );
    assert_eq!(
        ContentId::from_canonical_bytes(&observation.canonical_form().unwrap()),
        next
    );
}

#[test]
fn verdicts_name_an_already_recorded_model_observation() {
    let mut events = Events::new();
    let source = reply(&mut events);
    let mut verdict_ids = BTreeSet::new();
    for verdict in [
        ReplyVerdict::Answer,
        ReplyVerdict::Narration,
        ReplyVerdict::Question,
    ] {
        let id = append(
            &mut events,
            body(
                EventKind::Verdict { verdict },
                EventOrigin::Harness,
                &[source],
                1,
            ),
            &[source],
        );
        assert_eq!(events[&id].depth(), 1);
        assert_eq!(events[&id].parents(), &BTreeSet::from([source]));
        verdict_ids.insert(id);
    }
    assert_eq!(verdict_ids.len(), 3);
    let raw = events[verdict_ids.first().unwrap()].to_raw();
    assert!(Event::admit(raw, |_| None).is_err());
}

#[test]
fn generation_over_generated_material_is_refused_at_both_boundaries() {
    let mut events = Events::new();
    let source = reply(&mut events);
    let generated = intervention(&mut events, source);
    for declared in [0, 1, 2, u32::MAX] {
        assert_refused(
            body(
                EventKind::Intervention,
                EventOrigin::Harness,
                &[generated],
                declared,
            ),
            &[generated],
            &events,
        );
    }
}

#[test]
fn all_declared_depths_must_match_resolved_derivation_inputs() {
    let mut events = Events::new();
    let source = reply(&mut events);
    for depth in [1, 2, u32::MAX] {
        assert_refused(
            body(EventKind::Observation, EventOrigin::Model, &[], depth),
            &[],
            &events,
        );
    }
    for depth in [0, 2, u32::MAX] {
        assert_refused(
            body(
                EventKind::Intervention,
                EventOrigin::Harness,
                &[source],
                depth,
            ),
            &[source],
            &events,
        );
    }
}

#[test]
fn retrieval_preserves_generated_origin_and_depth() {
    let mut events = Events::new();
    let source = reply(&mut events);
    let generated = intervention(&mut events, source);
    let retrieval = append(
        &mut events,
        body(EventKind::Retrieval, EventOrigin::Harness, &[generated], 1),
        &[generated],
    );
    assert_eq!(events[&retrieval].body().origin, EventOrigin::Harness);
    assert_refused(
        body(EventKind::Retrieval, EventOrigin::Tool, &[generated], 1),
        &[generated],
        &events,
    );
    assert_refused(
        body(EventKind::Retrieval, EventOrigin::Harness, &[generated], 0),
        &[generated],
        &events,
    );
    assert_refused(
        body(
            EventKind::Intervention,
            EventOrigin::Harness,
            &[retrieval],
            1,
        ),
        &[retrieval],
        &events,
    );
}

#[test]
fn every_parent_and_source_must_resolve_to_the_named_event() {
    let mut events = Events::new();
    let source = reply(&mut events);
    let generated = intervention(&mut events, source);
    let missing = RootEvent::new(RootKind::UserAction, b"missing", 5)
        .id()
        .unwrap();
    assert_refused(
        body(EventKind::Observation, EventOrigin::Tool, &[], 0),
        &[missing],
        &events,
    );
    assert_refused(
        body(EventKind::Intervention, EventOrigin::Harness, &[missing], 1),
        &[missing],
        &events,
    );
    let raw = events[&generated].to_raw();
    assert!(Event::admit(raw, |_| events.get(&generated).cloned()).is_err());
}

#[test]
fn derivation_inputs_cannot_be_hidden_outside_the_causal_parent_set() {
    let mut events = Events::new();
    let source = reply(&mut events);
    assert_refused(
        body(EventKind::Intervention, EventOrigin::Harness, &[source], 1),
        &[],
        &events,
    );
    assert_refused(
        body(EventKind::Observation, EventOrigin::Model, &[source], 0),
        &[source],
        &events,
    );
}

#[test]
fn origin_and_source_cardinality_are_checked_for_each_operation() {
    let mut events = Events::new();
    let source = reply(&mut events);
    let tool = append(
        &mut events,
        body(EventKind::Observation, EventOrigin::Tool, &[], 0),
        &[],
    );
    let verdict = EventKind::Verdict {
        verdict: ReplyVerdict::Answer,
    };
    for kind in [EventKind::Intervention, verdict.clone()] {
        assert_refused(
            body(kind.clone(), EventOrigin::Model, &[source], 1),
            &[source],
            &events,
        );
    }
    assert_refused(
        body(verdict.clone(), EventOrigin::Harness, &[], 1),
        &[],
        &events,
    );
    assert_refused(
        body(EventKind::Observation, EventOrigin::Harness, &[], 0),
        &[],
        &events,
    );
    assert_refused(
        body(verdict.clone(), EventOrigin::Harness, &[tool], 1),
        &[tool],
        &events,
    );
    assert_refused(
        body(verdict.clone(), EventOrigin::Harness, &[source, tool], 1),
        &[source, tool],
        &events,
    );
    assert_refused(
        body(verdict, EventOrigin::Harness, &[source], 1),
        &[source, tool],
        &events,
    );
    assert_refused(
        body(EventKind::Retrieval, EventOrigin::Model, &[], 0),
        &[],
        &events,
    );
    assert_refused(
        body(EventKind::Retrieval, EventOrigin::Model, &[source, tool], 0),
        &[source, tool],
        &events,
    );
}

#[test]
fn a_rooted_harness_nudge_is_not_a_derivation_of_the_preceding_verdict() {
    let mut events = Events::new();
    let source = reply(&mut events);
    let verdict = append(
        &mut events,
        body(
            EventKind::Verdict {
                verdict: ReplyVerdict::Narration,
            },
            EventOrigin::Harness,
            &[source],
            1,
        ),
        &[source],
    );
    let nudge = append(
        &mut events,
        body(EventKind::Intervention, EventOrigin::Harness, &[], 1),
        &[verdict],
    );
    assert_eq!(events[&nudge].depth(), 1);
    assert!(events[&nudge].body().sources.is_empty());
}

#[test]
fn elision_keeps_the_existing_unit_proof_and_inherited_provenance() {
    let mut events = Events::new();
    let source = reply(&mut events);
    let unit = Unit::seal(
        Op::Elide,
        b"material",
        Span::new(0, 4),
        events[&source].body().root,
    )
    .unwrap();
    let event = append(
        &mut events,
        body(
            EventKind::Elision {
                unit: unit.id().unwrap(),
            },
            EventOrigin::Model,
            &[source],
            0,
        ),
        &[source],
    );
    assert_eq!(events[&event].depth(), 0);
    assert_eq!(
        events[&event]
            .verify_elision(&unit, &events[&source], b"material")
            .unwrap()
            .elided,
        unit.derivation().elided
    );
    assert!(events[&event]
        .verify_elision(&unit, &events[&source], b"changed")
        .is_err());
    let wrong = Unit::seal(Op::Elide, b"material", Span::new(4, 8), unit.root()).unwrap();
    assert!(events[&event]
        .verify_elision(&wrong, &events[&source], b"material")
        .is_err());
    assert!(events[&source]
        .verify_elision(&unit, &events[&source], b"material")
        .is_err());
}

#[test]
fn identity_commits_to_ordered_occurrence_and_every_causal_link() {
    let mut events = Events::new();
    let first = reply(&mut events);
    let mut next = events[&first].body().clone();
    next.seq += 1;
    let second = append(&mut events, next.clone(), &[]);
    assert_ne!(first, second);
    let parented = append(&mut events, next.clone(), &[first]);
    assert_ne!(second, parented);
    next.payload = RawContentId::from_content(b"different");
    assert_ne!(second, append(&mut events, next, &[]));
    let before = events[&first].canonical_form().unwrap();
    events.remove(&parented);
    assert_eq!(events[&first].canonical_form().unwrap(), before);
}

#[test]
fn a_retrieved_reply_cannot_be_passed_off_as_a_new_reply_to_adjudicate() {
    let mut events = Events::new();
    let source = reply(&mut events);
    let retrieved = append(
        &mut events,
        body(EventKind::Retrieval, EventOrigin::Model, &[source], 0),
        &[source],
    );
    assert_refused(
        body(
            EventKind::Verdict {
                verdict: ReplyVerdict::Answer,
            },
            EventOrigin::Harness,
            &[retrieved],
            1,
        ),
        &[retrieved],
        &events,
    );
}
