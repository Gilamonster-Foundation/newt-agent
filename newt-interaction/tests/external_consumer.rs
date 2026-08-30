//! **The external proof** (epic #1803, slice G1, #1934).
//!
//! Everything else in this crate tests Newt against Newt. This file tests
//! Newt against something that is not Newt: `conformance/responses.json`
//! is authored by `conformance/newt_conformance.py`, a stdlib-only Python
//! consumer with its own BLAKE3, its own DAG-CBOR encoder, and its own CID
//! renderer, written from the specifications. It has never seen this
//! crate's source and links against none of it.
//!
//! So the direction of proof matters. The Python side proves it can
//! *reproduce* what Rust minted (`newt_conformance.py verify`, run by CI).
//! This side proves the harder half: that bytes **Python authored** — a
//! record Rust has never seen — decode through the checked door, re-encode
//! byte-identically, mint the id Python predicted, and pass
//! `binding::validate_response`.
//!
//! If a foreign consumer had to import a Newt crate to produce an
//! acceptable response, that would be a fact about the protocol worth more
//! than this test passing. It does not.

use content_addressable::{canonical, ContentAddressable, ContentId};
use newt_interaction::{
    binding::{validate_response, HandlerId, RegisteredAction, ResponderContext},
    publish, Audience, ControlValue, HostMint, InteractionDefinition, InteractionInstance,
    OptionId, Refusal, Response,
};

/// Committed at build time rather than read at run time: a missing fixture
/// then fails to COMPILE, which is a stronger version of the golden-file
/// rule that a missing file must never be silently regenerated.
const EXTERNAL: &str = include_str!("../conformance/responses.json");
const VECTORS: &str = include_str!("data/interaction-vectors.json");

/// One record as the external consumer published it. Field-for-field the
/// same shape the Rust vectors use, because a consumer answering in a
/// private dialect would prove nothing.
#[derive(serde::Deserialize)]
struct Record {
    name: String,
    json: serde_json::Value,
    dagcbor_hex: String,
    content_id: String,
}

fn from_hex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "hex must be byte-aligned");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex digit"))
        .collect()
}

fn external() -> Vec<Record> {
    let records: Vec<Record> = serde_json::from_str(EXTERNAL).expect("external responses parse");
    assert!(
        !records.is_empty(),
        "the external consumer published no responses; every assertion below \
         would pass over an empty corpus"
    );
    records
}

/// The offer the external consumer was answering, decoded from the golden
/// vectors' own canonical bytes — not rebuilt in Rust. The consumer read
/// the same file.
fn offer() -> (InteractionDefinition, InteractionInstance) {
    let vectors: Vec<Record> = serde_json::from_str(VECTORS).expect("vectors parse");
    let mut definition = None;
    let mut instance = None;
    for v in &vectors {
        let bytes = from_hex(&v.dagcbor_hex);
        if v.name.starts_with("definition/") {
            definition = Some(canonical::from_canonical_dagcbor_checked(&bytes).expect("def"));
        } else if v.name.starts_with("instance/") {
            instance = Some(canonical::from_canonical_dagcbor_checked(&bytes).expect("inst"));
        }
    }
    (
        definition.expect("a definition vector"),
        instance.expect("an instance vector"),
    )
}

/// What the HOST brings. Handlers are deliberately not in the definition —
/// see `ResolvedAction`: "The CALLER's handler. Never derived from the
/// definition." So an external consumer can author a valid answer without
/// being able to know, or influence, what running it will invoke.
fn registered() -> Vec<RegisteredAction> {
    vec![RegisteredAction {
        option: OptionId::new("deny").unwrap(),
        handler: HandlerId::new("gate::deny").unwrap(),
        audiences: vec![Audience::Terminal, Audience::Web],
    }]
}

/// **The centrepiece.** Bytes authored outside Newt are accepted by Newt.
#[test]
fn every_externally_authored_response_is_accepted() {
    let (definition, instance) = offer();
    let lifecycle = publish(&HostMint::assert_host_authority(), &instance, &definition)
        .expect("the offer publishes");
    let actions = registered();
    let context = ResponderContext {
        workspace_key: &instance.scope.workspace_key,
        registered: &actions,
    };

    for record in external() {
        let bytes = from_hex(&record.dagcbor_hex);

        // 1. The foreign encoder's bytes pass the CHECKED door. A
        //    non-canonical encoding — an indefinite length, a non-smallest
        //    integer, a map key out of order — is refused here, so this
        //    step is where a plausible-looking foreign encoder fails.
        let response: Response = canonical::from_canonical_dagcbor_checked(&bytes)
            .unwrap_or_else(|e| panic!("`{}`: foreign bytes rejected: {e}", record.name));

        // 2. Re-encoding reproduces them exactly. Canonical means one
        //    encoding per value, in both directions.
        assert_eq!(
            response.canonical_form().expect("re-encode"),
            bytes,
            "`{}`: Rust re-encodes the foreign record differently",
            record.name
        );

        // 3. The id the consumer predicted is the id Rust mints.
        assert_eq!(
            ContentId::from_canonical_bytes(&bytes).to_string(),
            record.content_id,
            "`{}`: the consumer predicted a different id",
            record.name
        );

        // 4. Its JSON face and its CBOR face are the same record. The
        //    consumer publishes both; a consumer whose JSON disagreed with
        //    its own bytes would be documenting a record it did not send.
        let from_json: Response = serde_json::from_value(record.json.clone())
            .unwrap_or_else(|e| panic!("`{}`: JSON face does not decode: {e}", record.name));
        assert_eq!(
            from_json.canonical_form().expect("re-encode"),
            bytes,
            "`{}`: the published JSON and the published bytes are different records",
            record.name
        );

        // 5. And the protocol accepts it.
        let accepted = validate_response(&definition, &instance, &lifecycle, &response, &context)
            .unwrap_or_else(|e| panic!("`{}`: refused: {e}", record.name));
        assert_eq!(
            accepted.response.to_string(),
            record.content_id,
            "`{}`: accepted under a different id than it was published with",
            record.name
        );
    }
}

/// **Anti-vacuous twin, decode half.** One flipped byte must be caught.
///
/// Without this, step 1 above could be passing because the checked door
/// accepts anything shaped roughly right.
#[test]
fn a_perturbed_external_record_is_refused_or_reidentified() {
    for record in external() {
        let mut bytes = from_hex(&record.dagcbor_hex);
        let last = bytes.last_mut().expect("non-empty");
        *last ^= 0x01;

        // Either the flip makes the bytes undecodable, or it decodes to a
        // DIFFERENT record with a different id. What must never happen is
        // that it decodes to the same record under the same id.
        if let Ok(response) = canonical::from_canonical_dagcbor_checked::<Response>(&bytes) {
            assert_ne!(
                response.response_id().expect("id").to_string(),
                record.content_id,
                "`{}`: a flipped byte produced the same identity",
                record.name
            );
        }
    }
}

/// **Anti-vacuous twin, validation half.** `validate_response` must be
/// capable of refusing a record that arrived by exactly the same route.
///
/// The externally authored records are accepted above. If that acceptance
/// were unconditional, this test would fail — the mutation changes only
/// the VALUE's type, leaving the record well-formed, correctly addressed,
/// and bound to the same offer.
#[test]
fn a_type_confused_external_response_is_refused() {
    let (definition, instance) = offer();
    let lifecycle = publish(&HostMint::assert_host_authority(), &instance, &definition)
        .expect("the offer publishes");
    let actions = registered();
    let context = ResponderContext {
        workspace_key: &instance.scope.workspace_key,
        registered: &actions,
    };

    let mut mutated = 0;
    for record in external() {
        let bytes = from_hex(&record.dagcbor_hex);
        let mut response: Response =
            canonical::from_canonical_dagcbor_checked(&bytes).expect("decodes");

        // Answer the same control with a value of the wrong kind. A choice
        // becomes text; everything else becomes a choice.
        let submission = response.values.first_mut().expect("one submission");
        submission.value = match &submission.value {
            ControlValue::Choice { .. } => ControlValue::Text {
                text: "not a choice".to_string(),
            },
            _ => ControlValue::Choice {
                option: OptionId::new("deny").unwrap(),
            },
        };
        mutated += 1;

        match validate_response(&definition, &instance, &lifecycle, &response, &context) {
            Err(Refusal::WrongControlType { .. }) => {}
            Err(other) => panic!(
                "`{}`: refused, but for the wrong reason: {other}",
                record.name
            ),
            Ok(_) => panic!("`{}`: a value of the wrong type was accepted", record.name),
        }
    }
    assert!(
        mutated > 0,
        "no record was mutated; the twin proved nothing"
    );
}
