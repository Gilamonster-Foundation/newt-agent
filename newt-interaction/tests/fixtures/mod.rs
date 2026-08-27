//! Shared fixtures — each test binary includes the whole module and uses
//! the part it needs, so some are unused per binary.
#![allow(dead_code)]

//! Shared fixtures: one definition, one offer of it, one response to that
//! offer. Deliberately plain values so a failing identity assertion points
//! at the encoding, not at the fixture.

use newt_interaction::{
    AssertionKind, Audience, ChoiceOption, Control, ControlId, ControlKind, ControlValue,
    IdempotencyKey, InteractionDefinition, InteractionInstance, InteractionKind, Nonce, OptionId,
    Provenance, Requirement, ResponderPolicy, ResponderProvenance, Response, Revision, Scope,
    SemanticRole, Submission, RESPONSE_SCHEMA_V1,
};

#[must_use]
pub fn definition() -> InteractionDefinition {
    InteractionDefinition::new(
        InteractionKind::Choice,
        "⊘ run_command wants to run `bash`",
        vec![Control {
            id: ControlId::new("decision").unwrap(),
            kind: ControlKind::Choice {
                options: vec![
                    ChoiceOption {
                        id: OptionId::new("allow-once").unwrap(),
                        role: SemanticRole::Allow,
                        label: "allow once".to_string(),
                    },
                    ChoiceOption {
                        id: OptionId::new("deny").unwrap(),
                        role: SemanticRole::Deny,
                        label: "deny (default)".to_string(),
                    },
                ],
            },
            label: "what should happen".to_string(),
            requirement: Requirement::Required,
        }],
    )
}

#[must_use]
pub fn instance(def: &InteractionDefinition) -> InteractionInstance {
    InteractionInstance {
        schema: newt_interaction::INSTANCE_SCHEMA_V1.to_string(),
        nonce: Nonce::new("1756200000000000000-0f4c1b2e").unwrap(),
        definition: def.definition_id().unwrap(),
        revision: Revision::FIRST,
        ttl_ticks: 300,
        scope: Scope {
            workspace_key: "ws-abc".to_string(),
            conversation_id: "conv-1".to_string(),
        },
        responder_policy: ResponderPolicy {
            audiences: vec![Audience::Terminal],
            requires_assertion: false,
        },
        provenance: Provenance {
            origin: "permission-gate".to_string(),
            minted_tick: 42,
        },
    }
}

#[must_use]
pub fn response(def: &InteractionDefinition, inst: &InteractionInstance) -> Response {
    Response {
        schema: RESPONSE_SCHEMA_V1.to_string(),
        definition: def.definition_id().unwrap(),
        instance: inst.instance_id().unwrap(),
        revision: Revision::FIRST,
        values: vec![Submission {
            control: ControlId::new("decision").unwrap(),
            value: ControlValue::Choice {
                option: OptionId::new("deny").unwrap(),
            },
        }],
        idempotency_key: IdempotencyKey::new("first-try").unwrap(),
        responder_provenance: ResponderProvenance {
            kind: AssertionKind::SignedAssertion,
            subject: "operator:hartsock".to_string(),
            audience: Audience::Web,
            assertion: Some("assertion-handle-1".to_string()),
        },
    }
}
