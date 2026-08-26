//! **The schema tag is part of what a record IS** (A2.0, #1828).
//!
//! `newt_core::agentic::content_spill` already relies on this property for
//! spills — *"bumping it re-addresses every spill"* (`content_spill.rs:45-46`).
//! The same must hold here, or a v2 record could masquerade as the v1 it was
//! migrated from and a consumer would never see the difference.

use newt_interaction::{
    InteractionDefinition, ProtocolError, DEFINITION_SCHEMA_V1, INSTANCE_SCHEMA_V1,
    RESPONSE_SCHEMA_V1,
};

mod fixtures;
use fixtures::{definition, instance, response};

#[test]
fn the_schema_tag_is_bound_into_identity() {
    let base = definition();
    let base_id = base.definition_id().unwrap();
    let mut bumped = base.clone();
    bumped.schema = "newt.interaction.definition/v2".to_string();
    assert_ne!(
        bumped.definition_id().unwrap(),
        base_id,
        "bumping the schema tag must re-address the record"
    );

    let inst = instance(&base);
    let inst_id = inst.instance_id().unwrap();
    let mut inst_bumped = inst.clone();
    inst_bumped.schema = "newt.interaction.instance/v2".to_string();
    assert_ne!(inst_bumped.instance_id().unwrap(), inst_id);

    let resp = response(&base, &inst);
    let resp_id = resp.response_id().unwrap();
    let mut resp_bumped = resp.clone();
    resp_bumped.schema = "newt.interaction.response/v2".to_string();
    assert_ne!(resp_bumped.response_id().unwrap(), resp_id);
}

/// Every record is built carrying the current tag.
#[test]
fn a_freshly_built_record_carries_the_current_tag() {
    assert_eq!(definition().schema, DEFINITION_SCHEMA_V1);
    let def = definition();
    assert_eq!(instance(&def).schema, INSTANCE_SCHEMA_V1);
    assert_eq!(response(&def, &instance(&def)).schema, RESPONSE_SCHEMA_V1);
}

/// An unknown tag fails closed rather than being interpreted partially —
/// ADR law 5, and the same fail-closed dispatch `turn_chain.rs:96-103`
/// applies to `encoding_version`.
#[test]
fn an_unknown_schema_tag_fails_closed() {
    let mut def = definition();
    def.schema = "newt.interaction.definition/v99".to_string();
    let err = def.ensure_known_schema().unwrap_err();
    assert!(
        matches!(err, ProtocolError::UnknownSchema { .. }),
        "an unknown tag must refuse, not degrade silently: {err:?}"
    );
    // The error names both what was seen and what this build understands, so
    // an operator can tell a downgrade from corruption.
    let rendered = err.to_string();
    assert!(rendered.contains("v99") && rendered.contains(DEFINITION_SCHEMA_V1));

    let known = definition();
    assert!(known.ensure_known_schema().is_ok());
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
