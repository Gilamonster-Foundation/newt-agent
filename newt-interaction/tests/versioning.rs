//! **The schema tag is part of what a record IS** (A2.0, #1828).
//!
//! `newt_core::agentic::content_spill` already relies on this property for
//! spills — *"bumping it re-addresses every spill"* (`content_spill.rs:45-46`).
//! The same must hold here, or a v2 record could masquerade as the v1 it was
//! migrated from and a consumer would never see the difference.

use newt_interaction::{
    InteractionDefinition, DEFINITION_SCHEMA_V1, INSTANCE_SCHEMA_V1, RESPONSE_SCHEMA_V1,
};

mod fixtures;
use fixtures::{definition, instance, response};

/// The tag is part of what a record IS, asserted at the BYTES.
///
/// It can no longer be mutated in Rust — the tag is a type that
/// deserializes from exactly one value — so the property is checked where
/// it lives: patch the encoded tag and the content id moves. That is the
/// same guarantee `content_spill`'s `SPILL_SCHEMA_V1` relies on
/// ("bumping it re-addresses every spill"), now enforced by the type
/// system on the way in and by the encoding on the way out.
#[test]
fn the_schema_tag_is_bound_into_identity() {
    use content_addressable::{canonical, ContentAddressable, ContentId};

    let def = definition();
    let bytes = canonical::to_canonical_dagcbor(&def).unwrap();
    let id = def.content_id().unwrap();
    assert_eq!(ContentId::from_canonical_bytes(&bytes), id);

    // Same length, so the encoding stays structurally identical: only the
    // version digit changes.
    let at = bytes
        .windows(DEFINITION_SCHEMA_V1.len())
        .position(|w| w == DEFINITION_SCHEMA_V1.as_bytes())
        .expect("the tag is in the encoding");
    let mut bumped = bytes.clone();
    bumped[at + DEFINITION_SCHEMA_V1.len() - 1] = b'2';
    assert_ne!(
        ContentId::from_canonical_bytes(&bumped),
        id,
        "bumping the schema tag must re-address the record"
    );
}

/// Every record is built carrying the current tag.
#[test]
fn a_freshly_built_record_carries_the_current_tag() {
    let def = definition();
    assert_eq!(def.schema_tag(), DEFINITION_SCHEMA_V1);
    assert_eq!(instance(&def).schema_tag(), INSTANCE_SCHEMA_V1);
    assert_eq!(
        response(&def, &instance(&def)).schema_tag(),
        RESPONSE_SCHEMA_V1
    );
}

/// The tags are distinct per record type: a definition's bytes can never be
/// read as an instance's.
#[test]
fn the_three_tags_are_distinct() {
    let tags = [DEFINITION_SCHEMA_V1, INSTANCE_SCHEMA_V1, RESPONSE_SCHEMA_V1];
    let unique: std::collections::BTreeSet<_> = tags.iter().collect();
    assert_eq!(unique.len(), tags.len(), "schema tags collide: {tags:?}");
    for tag in tags {
        assert!(
            tag.starts_with("newt.interaction."),
            "{tag} breaks convention"
        );
        assert!(tag.ends_with("/v1"), "{tag} is not versioned");
    }
}

/// A definition round-trips through JSON with its tag intact — the shape a
/// non-Rust consumer sees in A2.1.
#[test]
fn a_definition_round_trips_through_json_with_its_tag() {
    let def = definition();
    let json = serde_json::to_string(&def).unwrap();
    assert!(json.contains(DEFINITION_SCHEMA_V1));
    let back: InteractionDefinition = serde_json::from_str(&json).unwrap();
    assert_eq!(back, def);
    assert_eq!(back.definition_id().unwrap(), def.definition_id().unwrap());
}
