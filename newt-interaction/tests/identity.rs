//! **Identity is derived, never assigned** (A2.0, #1828).
//!
//! Every record here is a canonical structured value, so its id must be a
//! `ContentId` minted through `ContentAddressable` — and it must commit to
//! the WHOLE record. A canonical form over a hand-picked subset is the
//! classic first cut and the classic defect: two records that differ in a
//! field the encoder never saw would share an id, and an "exact form digest"
//! that is not exact is worse than none.

use content_addressable::{canonical, ContentAddressable, ContentId};
use newt_interaction::{
    Control, ControlKind, DefinitionId, FeatureDemand, InteractionDefinition, InteractionInstance,
    InteractionKind, Requirement, Response, SemanticRole, SurfaceFeature,
};

mod fixtures;
use fixtures::{definition, instance, response};

/// One mutation case: the field it perturbs, and how.
type Case<T> = (&'static str, fn(&mut T));

/// Each record's id is exactly what the crate's own primitives produce from
/// its canonical bytes — there is no second minting path.
#[test]
fn every_record_mints_through_content_addressable() {
    let def = definition();
    assert_eq!(
        def.definition_id().unwrap().content_id(),
        &ContentId::from_canonical_bytes(&def.canonical_form().unwrap()),
    );
    let inst = instance(&def);
    assert_eq!(
        inst.instance_id().unwrap().content_id(),
        &ContentId::from_canonical_bytes(&inst.canonical_form().unwrap()),
    );
    let resp = response(&def, &inst);
    assert_eq!(
        resp.response_id().unwrap().content_id(),
        &ContentId::from_canonical_bytes(&resp.canonical_form().unwrap()),
    );
}

/// Determinism is a property of the dag-cbor ENCODER, not of caller
/// discipline: equal values produce equal bytes regardless of how the value
/// was assembled. Asserted rather than assumed.
#[test]
fn equal_values_have_equal_ids_and_field_order_is_irrelevant() {
    let a = definition();
    let mut b = InteractionDefinition::new(a.kind, a.markdown.clone(), Vec::new());
    // Assemble the same value by a different route.
    b.controls = a.controls.clone();
    b.revision = a.revision;
    b.features = a.features.clone();
    assert_eq!(a, b);
    assert_eq!(a.definition_id().unwrap(), b.definition_id().unwrap());
    assert_eq!(
        canonical::to_canonical_dagcbor(&a).unwrap(),
        canonical::to_canonical_dagcbor(&b).unwrap(),
    );
}

/// An id parses only from its OWN canonical rendering. Accepting an
/// alternate spelling would let two strings name one record — the gate
/// `SpillCid::parse` already applies in newt-core (`content_spill.rs:155-167`).
#[test]
fn ids_do_not_parse_from_non_canonical_presentation() {
    let id = definition().definition_id().unwrap();
    let canonical_text = id.to_string();
    assert_eq!(DefinitionId::parse(&canonical_text).unwrap(), id);

    for spelling in [
        canonical_text.to_uppercase(),
        format!(" {canonical_text}"),
        format!("{canonical_text} "),
    ] {
        assert!(
            DefinitionId::parse(&spelling).is_err(),
            "a non-canonical presentation must not parse: {spelling:?}"
        );
    }
    assert!(DefinitionId::parse("not-a-content-id").is_err());
}

/// The nonce ROUTES; it is not the identity. Two offers of the same
/// definition under the same policy are still two different instances,
/// because the nonce is part of what the instance record commits to.
#[test]
fn the_instance_nonce_is_not_the_identity() {
    let def = definition();
    let first = instance(&def);
    let mut second = first.clone();
    second.nonce = newt_interaction::Nonce::new("a-different-handle").unwrap();

    assert_ne!(
        first.instance_id().unwrap(),
        second.instance_id().unwrap(),
        "the nonce must be bound into the instance's identity"
    );
    // ...and the id is not merely the nonce restated.
    assert_ne!(
        first.instance_id().unwrap().to_string(),
        first.nonce.as_str()
    );
}

/// **Every semantic field of an offer moves its id — enumerated, not
/// sampled.**
///
/// The table below is paired with an EXHAUSTIVE destructure of the record.
/// Adding a field to `InteractionInstance` fails to compile here until it
/// is named, and naming it without adding a mutation case is then a
/// deliberate, visible act rather than an oversight. Sampling a few fields
/// and asserting "the id covers everything" is the vacuous shape: it
/// passes just as well when the encoder skips the field nobody perturbed.
#[test]
fn every_semantic_field_of_an_offer_moves_its_id() {
    let def = definition();
    let base = instance(&def);

    // EXHAUSTIVE — no `..`. This is the compile-time half of the guard.
    // `schema` has no mutation case below: it is a TYPE that deserializes
    // from exactly one value, so a differently-tagged instance is not
    // constructible in Rust. The property it used to check lives in
    // `versioning::the_schema_tag_is_bound_into_identity`, which patches
    // the tag in the ENCODED bytes and asserts the id moves.
    let InteractionInstance {
        schema: _,
        nonce: _,
        definition: _,
        revision: _,
        ttl_ticks: _,
        scope: _,
        responder_policy: _,
        provenance: _,
    } = &base;

    let cases: Vec<Case<InteractionInstance>> = vec![
        ("nonce", |i| {
            i.nonce = newt_interaction::Nonce::new("another-handle").unwrap();
        }),
        ("definition", |i| {
            let mut other = definition();
            other.markdown.push('?');
            i.definition = other.definition_id().unwrap();
        }),
        ("revision", |i| i.revision = i.revision.next()),
        ("ttl_ticks", |i| i.ttl_ticks += 1),
        ("scope.workspace_key", |i| {
            i.scope.workspace_key = "somewhere-else".into();
        }),
        ("scope.conversation_id", |i| {
            i.scope.conversation_id = "another-conversation".into();
        }),
        ("responder_policy.audiences", |i| {
            i.responder_policy.audiences = vec![newt_interaction::Audience::Web];
        }),
        ("responder_policy.requires_assertion", |i| {
            i.responder_policy.requires_assertion = !i.responder_policy.requires_assertion;
        }),
        ("provenance.origin", |i| {
            i.provenance.origin = "someone-else".into();
        }),
        ("provenance.minted_tick", |i| i.provenance.minted_tick += 1),
    ];

    let base_id = base.instance_id().unwrap();
    for (field, mutate) in cases {
        let mut altered = base.clone();
        mutate(&mut altered);
        assert_ne!(altered, base, "the `{field}` case mutated nothing");
        assert_ne!(
            altered.instance_id().unwrap(),
            base_id,
            "`{field}` is not bound into the instance's identity"
        );
    }
}

/// **`InstanceId` is the identity of the OFFER, never of its state.**
///
/// ADR laws 8 and 12 put instance state out of band: definition and
/// transcript bytes are immutable, and progress, expiry, responses, and
/// resolution travel in digest-bound sidecars. Carrying a lifecycle field
/// inside the content-addressed record breaks that — a Published X becomes
/// an Answered Y, so the id names a snapshot, and a response that bound X
/// no longer refers to anything the store holds. A3's transition records
/// reference this STABLE id instead.
#[test]
fn instance_identity_is_stable_across_lifecycle() {
    let inst = instance(&definition());
    let wire = serde_json::to_value(&inst).unwrap();
    assert!(
        wire.get("lifecycle").is_none(),
        "the offer record carries lifecycle state, so its id is a snapshot \
         id: {wire}"
    );
    // The type still exists — A3's out-of-band records use it — it simply
    // does not live inside the thing whose identity must not move.
    let _ = newt_interaction::LifecycleState::Published;
}

/// **A response binds every field it claims to — enumerated, not sampled.**
///
/// Same shape as the offer table, and the same reason: the ADR's list is
/// "type + definition + instance + digest + revision + control values +
/// idempotency key + responder provenance", and a test that perturbs four
/// of those proves nothing about the other four.
#[test]
fn every_field_a_response_claims_to_bind_moves_its_id() {
    let def = definition();
    let inst = instance(&def);
    let base = response(&def, &inst);

    // EXHAUSTIVE — no `..`.
    // See the note above: `schema` is type-pinned, so its identity
    // binding is asserted at the bytes rather than here.
    let Response {
        schema: _,
        definition: _,
        instance: _,
        revision: _,
        values: _,
        idempotency_key: _,
        responder_provenance: _,
    } = &base;

    let cases: Vec<Case<Response>> = vec![
        ("definition", |r| {
            let mut other = definition();
            other.markdown.push('?');
            r.definition = other.definition_id().unwrap();
        }),
        ("instance", |r| {
            let mut other = instance(&definition());
            other.ttl_ticks += 7;
            r.instance = other.instance_id().unwrap();
        }),
        ("revision", |r| r.revision = r.revision.next()),
        ("values.control", |r| {
            r.values[0].control = newt_interaction::ControlId::new("other-field").unwrap();
        }),
        ("values.value", |r| {
            r.values[0].value = newt_interaction::ControlValue::Text {
                text: "typed instead".into(),
            };
        }),
        ("values.len", |r| {
            r.values.push(newt_interaction::Submission {
                control: newt_interaction::ControlId::new("other-field").unwrap(),
                value: newt_interaction::ControlValue::Toggle { on: true },
            });
        }),
        ("idempotency_key", |r| {
            r.idempotency_key = newt_interaction::IdempotencyKey::new("second-try").unwrap();
        }),
        ("responder_provenance.kind", |r| {
            r.responder_provenance.kind = newt_interaction::AssertionKind::Unauthenticated;
        }),
        ("responder_provenance.subject", |r| {
            r.responder_provenance.subject = "someone-else".into();
        }),
        ("responder_provenance.audience", |r| {
            r.responder_provenance.audience = newt_interaction::Audience::Terminal;
        }),
        ("responder_provenance.assertion", |r| {
            r.responder_provenance.assertion = None;
        }),
    ];

    let base_id = base.response_id().unwrap();
    for (field, mutate) in cases {
        let mut altered = base.clone();
        mutate(&mut altered);
        assert_ne!(altered, base, "the `{field}` case mutated nothing");
        assert_ne!(
            altered.response_id().unwrap(),
            base_id,
            "`{field}` is not bound into the response's identity"
        );
    }
}

/// Same guard for the definition: exhaustive destructure plus a case per
/// field.
#[test]
fn every_semantic_field_of_a_definition_moves_its_id() {
    let base = definition();

    // EXHAUSTIVE — no `..`.
    // See the note above: `schema` is type-pinned, so its identity
    // binding is asserted at the bytes rather than here.
    let InteractionDefinition {
        schema: _,
        kind: _,
        revision: _,
        markdown: _,
        controls: _,
        features: _,
    } = &base;

    let cases: Vec<Case<InteractionDefinition>> = vec![
        ("kind", |d| d.kind = InteractionKind::Confirm),
        ("revision", |d| d.revision = d.revision.next()),
        ("markdown", |d| d.markdown.push('!')),
        ("controls.label", |d| d.controls[0].label.push('!')),
        ("controls.kind", |d| d.controls[0].kind = ControlKind::Text),
        // The options are part of the field. A definition whose option set
        // could change without changing its id would let an offer be
        // re-aimed at answers its author never wrote.
        ("controls.option.id", |d| {
            if let ControlKind::Choice { options } = &mut d.controls[0].kind {
                options[0].id = newt_interaction::OptionId::new("renamed-option").unwrap();
            }
        }),
        ("controls.option.role", |d| {
            if let ControlKind::Choice { options } = &mut d.controls[0].kind {
                options[0].role = SemanticRole::Cancel;
            }
        }),
        ("controls.option.label", |d| {
            if let ControlKind::Choice { options } = &mut d.controls[0].kind {
                options[0].label.push('!');
            }
        }),
        ("controls.option.len", |d| {
            if let ControlKind::Choice { options } = &mut d.controls[0].kind {
                options.push(newt_interaction::ChoiceOption {
                    id: newt_interaction::OptionId::new("extra").unwrap(),
                    role: SemanticRole::Cancel,
                    label: "back".to_string(),
                });
            }
        }),
        ("controls.requirement", |d| {
            d.controls[0].requirement = Requirement::Optional;
        }),
        ("controls.id", |d| {
            d.controls[0].id = newt_interaction::ControlId::new("renamed").unwrap();
        }),
        ("controls.len", |d| {
            d.controls.push(Control {
                id: newt_interaction::ControlId::new("extra").unwrap(),
                kind: ControlKind::Text,
                label: "a second field".to_string(),
                requirement: Requirement::Optional,
            });
        }),
        ("features.len", |d| {
            d.features.push(FeatureDemand {
                feature: SurfaceFeature::new(SurfaceFeature::DIAGRAMS).unwrap(),
                requirement: Requirement::Optional,
            });
        }),
        ("features.feature", |d| {
            d.features = vec![FeatureDemand {
                feature: SurfaceFeature::new(SurfaceFeature::SECRET_INPUT).unwrap(),
                requirement: Requirement::Optional,
            }];
        }),
        ("features.requirement", |d| {
            d.features = vec![FeatureDemand {
                feature: SurfaceFeature::new(SurfaceFeature::DIAGRAMS).unwrap(),
                requirement: Requirement::Required,
            }];
        }),
    ];

    let base_id = base.definition_id().unwrap();
    for (field, mutate) in cases {
        let mut altered = base.clone();
        mutate(&mut altered);
        assert_ne!(altered, base, "the `{field}` case mutated nothing");
        assert_ne!(
            altered.definition_id().unwrap(),
            base_id,
            "`{field}` is not bound into the definition's identity"
        );
    }
}
