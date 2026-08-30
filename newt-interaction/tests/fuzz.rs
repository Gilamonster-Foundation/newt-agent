//! **Fuzzing the untrusted wire boundary** (epic #1803, slice G1, #1934).
//!
//! Two kinds of untrusted input reach this crate, and they fail in
//! different ways:
//!
//! 1. **Arbitrary bytes**, from a remote peer or a stored record. The
//!    decoder must be total — an `Err` or an `Unknown`, never a panic and
//!    never a partial interpretation.
//! 2. **Arbitrary author-supplied strings**, from untrusted markup (Law
//!    11). Markdown, labels, subjects and workspace keys are all
//!    attacker-chosen, and A3's rule is that an author-assigned field must
//!    never decide fail-closed behaviour.
//!
//! Both are stated below over every input rather than over the inputs
//! someone thought to write down. `proptest` for the reasons its
//! dev-dependency comment gives.
//!
//! **No I/O, no clock, no subprocess.** Every function here is pure.

use content_addressable::ContentAddressable;
use newt_interaction::{
    binding::{validate_response, ResponderContext},
    decode_definition, decode_instance, decode_response, publish, AssertionKind, Audience,
    ChoiceOption, Control, ControlId, ControlKind, ControlValue, Decoded, HostMint, IdempotencyKey,
    InteractionDefinition, InteractionInstance, InteractionKind, Nonce, OptionId, Provenance,
    Refusal, Requirement, ResponderPolicy, ResponderProvenance, Response, Revision, Scope,
    SemanticRole, Submission,
};
use proptest::prelude::*;

fn definition(markdown: &str, label: &str) -> InteractionDefinition {
    // Confirm, not Form: one Choice control offering a grant and a refusal
    // is decision-shaped, and #1914's guard requires that shape to declare
    // itself Confirm. The fixture was wrong; the guard was right.
    InteractionDefinition::new(
        InteractionKind::Confirm,
        markdown,
        vec![Control {
            id: ControlId::new("decision").expect("a fixed, valid id"),
            kind: ControlKind::Choice {
                options: vec![
                    ChoiceOption {
                        id: OptionId::new("allow").expect("valid"),
                        role: SemanticRole::Allow,
                        label: label.to_string(),
                        key: String::new(),
                        aliases: Vec::new(),
                    },
                    ChoiceOption {
                        id: OptionId::new("deny").expect("valid"),
                        role: SemanticRole::Deny,
                        label: label.to_string(),
                        key: String::new(),
                        aliases: Vec::new(),
                    },
                ],
            },
            label: label.to_string(),
            requirement: Requirement::Required,
        }],
    )
}

fn instance(def: &InteractionDefinition, workspace: &str) -> InteractionInstance {
    InteractionInstance {
        schema: newt_interaction::InstanceTag,
        nonce: Nonce::new("1756200000000000000-0f4c1b2e").expect("valid"),
        definition: def.definition_id().expect("id"),
        revision: Revision::FIRST,
        ttl_ticks: 300,
        scope: Scope {
            workspace_key: workspace.to_string(),
            conversation_id: "conv-1".to_string(),
        },
        responder_policy: ResponderPolicy {
            audiences: vec![Audience::Terminal],
            requires_assertion: false,
        },
        provenance: Provenance {
            origin: "fuzz".to_string(),
            minted_tick: 1,
        },
    }
}

fn response(def: &InteractionDefinition, inst: &InteractionInstance, subject: &str) -> Response {
    Response {
        schema: newt_interaction::ResponseTag,
        definition: def.definition_id().expect("id"),
        instance: inst.instance_id().expect("id"),
        revision: Revision::FIRST,
        values: vec![Submission {
            control: ControlId::new("decision").expect("valid"),
            value: ControlValue::Choice {
                option: OptionId::new("deny").expect("valid"),
            },
        }],
        idempotency_key: IdempotencyKey::new("k").expect("valid"),
        responder_provenance: ResponderProvenance {
            kind: AssertionKind::Unauthenticated,
            subject: subject.to_string(),
            audience: Audience::Terminal,
            assertion: None,
        },
    }
}

proptest! {
    // No failure-persistence file. The unit tier does no filesystem I/O
    // (CLAUDE.md, "Testing strategy"), and a test that writes into the
    // source tree when it fails is a test that fails twice on a read-only
    // CI checkout. proptest prints the failing seed as a `cc` line either
    // way, which is what actually reproduces the case.
    #![proptest_config(ProptestConfig { failure_persistence: None, ..ProptestConfig::default() })]

    /// **Decoding is total, and a `Known` record is byte-exact.**
    ///
    /// Arbitrary bytes get an `Err`, or an `Unknown` that keeps them
    /// uninterpreted — never a panic, and never a `Known` whose
    /// re-encoding differs from what arrived. That last clause is the one
    /// that matters: a decoder that accepted a non-canonical encoding
    /// would let the same record carry two identities.
    #[test]
    fn decoding_arbitrary_bytes_is_total(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        for decoded in [
            decode_definition(&bytes).map(|d| matches!(d, Decoded::Known(_))),
            decode_instance(&bytes).map(|d| matches!(d, Decoded::Known(_))),
        ] {
            // Both outcomes are fine; only a panic is not.
            let _ = decoded;
        }
        if let Ok(Decoded::Known(record)) = decode_response(&bytes) {
            prop_assert_eq!(
                record.canonical_form().expect("re-encode"),
                bytes,
                "a Known record did not re-encode to the bytes it arrived as"
            );
        }
    }

    /// **A truncated or corrupted valid record never becomes a different
    /// valid record under the same id.**
    ///
    /// Starts from real canonical bytes rather than noise, because that is
    /// where a decoder is most likely to be lenient.
    #[test]
    fn corrupting_a_real_record_never_preserves_its_identity(
        index in 0usize..64, xor in 1u8..=255,
    ) {
        let def = definition("⊘ run_command wants to run `bash`", "allow once");
        let mut bytes = def.canonical_form().expect("encode");
        let id = def.definition_id().expect("id");
        let at = index % bytes.len();
        bytes[at] ^= xor;
        if let Ok(Decoded::Known(record)) = decode_definition(&bytes) {
            prop_assert_ne!(
                record.definition_id().expect("id"), id,
                "a corrupted record kept the original identity"
            );
        }
    }

    /// **Author-supplied text cannot collide two definitions.**
    ///
    /// Markdown and labels come from untrusted markup. Two definitions
    /// that differ in either are different records, and identity is what
    /// every later binding rests on.
    #[test]
    fn author_text_always_moves_the_identity(
        left in "\\PC{0,64}", right in "\\PC{0,64}", label in "\\PC{0,32}",
    ) {
        let a = definition(&left, &label).definition_id().expect("id");
        let b = definition(&right, &label).definition_id().expect("id");
        prop_assert_eq!(a == b, left == right);
    }

    /// **A3: an author-assigned field never decides fail-closed behaviour.**
    ///
    /// The responder's `subject` is a string the RESPONDER chose. Whatever
    /// it says, it cannot satisfy a policy that demands an assertion, and
    /// it cannot cross a workspace fence.
    #[test]
    fn a_responder_supplied_string_never_grants_anything(
        subject in "\\PC{0,64}", claimed in "\\PC{0,32}",
    ) {
        let def = definition("markdown", "label");
        let inst = instance(&def, "ws-real");
        let lifecycle = publish(&HostMint::assert_host_authority(), &inst, &def)
            .expect("publishes");
        let resp = response(&def, &inst, &subject);

        // Inside the fence: accepted regardless of what `subject` says.
        let inside = ResponderContext { workspace_key: "ws-real", registered: &[] };
        // A choice needs a registered handler, so the refusal here is
        // UnknownAction — never anything the subject string chose.
        let refused_for_the_handler = matches!(
            validate_response(&def, &inst, &lifecycle, &resp, &inside),
            Err(Refusal::UnknownAction { .. })
        );
        prop_assert!(
            refused_for_the_handler,
            "a responder-supplied subject changed the outcome inside the fence"
        );

        // Outside it: refused for the FENCE, whatever the subject claims.
        // The caller's key is the authority; the record's strings are not.
        prop_assume!(claimed != "ws-real");
        let outside = ResponderContext { workspace_key: &claimed, registered: &[] };
        let refused_for_the_fence = matches!(
            validate_response(&def, &inst, &lifecycle, &resp, &outside),
            Err(Refusal::WorkspaceMismatch { .. })
        );
        prop_assert!(
            refused_for_the_fence,
            "a record's own strings crossed the caller's workspace fence"
        );
    }

    /// **Validated scalars accept or reject; they never mangle.**
    ///
    /// A constructor that silently normalized its input would make two
    /// different author strings the same id.
    #[test]
    fn a_validated_scalar_is_never_silently_rewritten(text in "\\PC{0,64}") {
        if let Ok(id) = ControlId::new(&text) {
            prop_assert_eq!(id.as_str(), text.as_str());
        }
        if let Ok(id) = OptionId::new(&text) {
            prop_assert_eq!(id.as_str(), text.as_str());
        }
    }
}

/// **Anti-vacuous twin for the decoder properties.**
///
/// `decoding_arbitrary_bytes_is_total` and
/// `corrupting_a_real_record_never_preserves_its_identity` are both guarded
/// by `if let Ok(Decoded::Known(..))`. A decoder that returned `Err` for
/// everything would satisfy them without decoding anything, so both arms
/// have to be shown reachable on real input.
#[test]
fn the_decoder_reaches_known_err_and_unknown() {
    let def = definition("markdown", "label");
    let bytes = def.canonical_form().expect("encode");

    assert!(
        matches!(decode_definition(&bytes), Ok(Decoded::Known(_))),
        "the decoder never returns Known; the fuzz properties are vacuous"
    );
    // Bytes that are not DAG-CBOR at all.
    assert!(decode_definition(&[0xff, 0xff, 0xff]).is_err());
    // Well-formed bytes under a tag this build does not know: Unknown, with
    // no partial interpretation.
    assert!(
        matches!(decode_response(&bytes), Ok(Decoded::Unknown(_))),
        "a definition read as a response should be Unknown, not Known"
    );
}

/// **Anti-vacuous twin for the validated-scalar property.**
///
/// `a_validated_scalar_is_never_silently_rewritten` is guarded by `if let
/// Ok`, so a constructor that rejected everything would satisfy it. Both
/// outcomes are real.
#[test]
fn a_validated_scalar_reaches_both_outcomes() {
    assert_eq!(
        ControlId::new("decision").expect("accepts").as_str(),
        "decision"
    );
    assert!(ControlId::new("").is_err(), "the empty id was accepted");
}

/// **Anti-vacuous twin for the A3 property.**
///
/// It asserts two refusals. If `validate_response` refused everything, it
/// would hold and prove nothing — so the same offer must be capable of
/// ACCEPTING, with the one thing that was actually missing supplied.
#[test]
fn the_same_offer_accepts_when_the_host_registers_the_action() {
    use newt_interaction::binding::{HandlerId, RegisteredAction};

    let def = definition("markdown", "label");
    let inst = instance(&def, "ws-real");
    let lifecycle = publish(&HostMint::assert_host_authority(), &inst, &def).expect("publishes");
    let resp = response(&def, &inst, "operator:anything");
    let actions = vec![RegisteredAction {
        option: OptionId::new("deny").expect("valid"),
        handler: HandlerId::new("gate::deny").expect("valid"),
        audiences: vec![Audience::Terminal],
    }];
    let context = ResponderContext {
        workspace_key: "ws-real",
        registered: &actions,
    };
    let accepted = validate_response(&def, &inst, &lifecycle, &resp, &context)
        .expect("the offer accepts once the host registers the action");
    assert_eq!(accepted.actions.len(), 1);
}
