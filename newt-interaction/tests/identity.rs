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
    Control, ControlKind, DefinitionId, InteractionDefinition, InteractionKind, SemanticRole,
};

mod fixtures;
use fixtures::{definition, instance, response};

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

/// The canonical form must cover the WHOLE record. Flipping any single field
/// must move the id; a field the encoder skips is a field an attacker can
/// change for free.
#[test]
fn a_changed_field_changes_the_definition_id() {
    let base = definition();
    let base_id = base.definition_id().unwrap();

    let mut kind = base.clone();
    kind.kind = InteractionKind::Confirm;
    assert_ne!(kind.definition_id().unwrap(), base_id, "kind is unbound");

    let mut revision = base.clone();
    revision.revision = base.revision.next();
    assert_ne!(
        revision.definition_id().unwrap(),
        base_id,
        "revision is unbound"
    );

    let mut features = base.clone();
    features.features.secret_input = true;
    assert_ne!(
        features.definition_id().unwrap(),
        base_id,
        "surface features are unbound"
    );

    let mut markdown = base.clone();
    markdown.markdown.push('!');
    assert_ne!(
        markdown.definition_id().unwrap(),
        base_id,
        "markdown is unbound"
    );

    let mut controls = base.clone();
    controls.controls.push(Control {
        id: newt_interaction::ControlId::new("extra").unwrap(),
        role: SemanticRole::Cancel,
        kind: ControlKind::Choice,
        label: "back".to_string(),
        required: false,
    });
    assert_ne!(
        controls.definition_id().unwrap(),
        base_id,
        "controls are unbound"
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
    b.features = a.features;
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

/// Every mutable axis of an offer is bound: scope, TTL, provenance, and
/// lifecycle all move the id. An offer whose fence could change without
/// changing its identity is an offer that can be silently re-aimed.
#[test]
fn every_binding_of_an_offer_is_covered() {
    let def = definition();
    let base = instance(&def);
    let base_id = base.instance_id().unwrap();

    let mut ttl = base.clone();
    ttl.ttl_ticks += 1;
    assert_ne!(ttl.instance_id().unwrap(), base_id, "ttl is unbound");

    let mut scope = base.clone();
    scope.scope.workspace_key = "somewhere-else".to_string();
    assert_ne!(scope.instance_id().unwrap(), base_id, "scope is unbound");

    let mut provenance = base.clone();
    provenance.provenance.origin = "someone-else".to_string();
    assert_ne!(
        provenance.instance_id().unwrap(),
        base_id,
        "provenance is unbound"
    );

    let mut lifecycle = base.clone();
    lifecycle.lifecycle = newt_interaction::LifecycleState::Published;
    assert_ne!(
        lifecycle.instance_id().unwrap(),
        base_id,
        "lifecycle is unbound"
    );
}

/// A response binds the ADR's full list. The idempotency key and the
/// responder are part of it: the same values submitted twice under
/// different keys are two submissions, and the same submission claimed by a
/// different audience is a different record.
#[test]
fn a_response_binds_everything_it_claims_to() {
    let def = definition();
    let inst = instance(&def);
    let base = response(&def, &inst);
    let base_id = base.response_id().unwrap();

    let mut definition_swap = base.clone();
    let mut other_def = def.clone();
    other_def.markdown.push('?');
    definition_swap.definition = other_def.definition_id().unwrap();
    assert_ne!(
        definition_swap.response_id().unwrap(),
        base_id,
        "the definition digest is unbound"
    );

    let mut revision = base.clone();
    revision.revision = base.revision.next();
    assert_ne!(
        revision.response_id().unwrap(),
        base_id,
        "revision is unbound"
    );

    let mut key = base.clone();
    key.idempotency_key = newt_interaction::IdempotencyKey::new("second-try").unwrap();
    assert_ne!(
        key.response_id().unwrap(),
        base_id,
        "the idempotency key is unbound"
    );

    let mut responder = base.clone();
    responder.responder = newt_interaction::Audience::Terminal;
    assert_ne!(
        responder.response_id().unwrap(),
        base_id,
        "the responder is unbound"
    );

    // #1828 §3.2 and the ADR bind a response to "... idempotency key +
    // responder provenance". An audience is not provenance: it says which
    // KIND of surface answered, not which authenticated party did.
    let mut provenance = base.clone();
    provenance.responder_provenance.subject = "someone-else".to_string();
    assert_ne!(
        provenance.response_id().unwrap(),
        base_id,
        "responder provenance is unbound"
    );
}
