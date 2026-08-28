//! **The permission decision, on the A3 controller** (B0b-1, #1842).
//!
//! B0a made both surfaces build one [`InteractionDefinition`]. This module
//! makes the ACCEPT/DENY decision itself run through
//! [`newt_interaction::validate_response`], on both surfaces.
//!
//! # What moved, and what did not
//!
//! [`Question::parse`](crate::Question::parse) is **kept**, demoted to what
//! it always actually was: an input DECODER. It turns an operator's
//! keystroke into a candidate action, applying the alias and
//! ambiguity-denial rules that are a presentation concern (`a` vs `A` are
//! distinct keys; an alias never shadows a real key; two matches deny).
//! What it no longer does is AUTHORIZE. That is now
//! `validate_response`'s: the candidate action is expressed as a
//! [`Response`] against the published definition, and only an `Accepted`
//! authorizes.
//!
//! The defence this buys is not theoretical. The registry of executable
//! actions is supplied by the CALLER and is INDEPENDENT of the definition
//! (epic law 13), so an action the definition offers but the caller never
//! registered is refused, and an action registered for the terminal is
//! refused when it arrives from the web — even if the form displayed it.
//!
//! # No persistence here
//!
//! Deliberately: B0b-1 changes no schema. The consequence is worth stating
//! precisely, because it bounds what the web surface proves. The
//! DEFINITION binding is authoritative on both surfaces — a submission
//! naming an action the published form does not carry is refused
//! everywhere. The INSTANCE binding is locally minted at answer time on the
//! web, so it is self-consistent rather than a proof that the answer
//! reached the offer the gate actually published. Persisting the instance,
//! and with it that cross-process binding, is #1846.

use newt_interaction::binding::{Refusal, RegisteredAction, ResponderContext};
use newt_interaction::lifecycle::{publish, HostMint, Lifecycle, LifecycleError};
use newt_interaction::{
    AssertionKind, Audience, ControlId, ControlValue, IdempotencyKey, InteractionDefinition,
    InteractionInstance, Nonce, OptionId, ProtocolError, Provenance, ResponderPolicy,
    ResponderProvenance, Response, Scope, Submission,
};

use crate::interaction_adapter::DECISION_CONTROL;
use crate::PermissionAction;

/// How long an offer stays answerable, in the same nanosecond ticks the
/// store counts.
///
/// **The TTL reconciliation** (#1842). Two independent wall-clock
/// constants used to exist with nothing relating them: the store's
/// `PERMISSION_REQUEST_TTL_NANOS` (5 min) and the gate's
/// `web_decision_timeout` (4 min). The gate timeout being the shorter one
/// is load bearing — the gate must give up while the offer is still
/// answerable, or a decision could land against a row the store has
/// already aged out — and it was untested. An offer now carries ONE
/// number, taken from the store's, and
/// `b0b::the_gate_timeout_is_shorter_than_the_store_ttl` asserts the
/// relationship instead of leaving it to coincidence.
#[must_use]
pub fn offer_ttl_ticks() -> i64 {
    crate::ConversationStore::PERMISSION_REQUEST_TTL_NANOS
}

/// The host's tick, in the same units the store's claim clock uses.
#[must_use]
pub fn now_tick() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
}

/// Mint a host offer of `definition`, and publish it.
///
/// The nonce is fresh and unguessable: it is a routing handle that travels
/// beside the address, and possession of it authorizes nothing.
///
/// # Errors
///
/// [`LifecycleError`] when the offer does not bind the definition
/// presented — which cannot happen for an instance minted here, and is
/// propagated rather than unwrapped because this sits on the permission
/// path.
pub fn mint_offer(
    definition: &InteractionDefinition,
    workspace_key: &str,
    conversation_id: &str,
    audience: Audience,
    minted_tick: i64,
) -> Result<(InteractionInstance, Lifecycle), LifecycleError> {
    let nonce = Nonce::new(format!("{minted_tick}-{}", uuid::Uuid::new_v4().simple()))
        .map_err(LifecycleError::Protocol)?;
    let instance = InteractionInstance {
        schema: newt_interaction::InstanceTag,
        nonce,
        definition: definition
            .definition_id()
            .map_err(LifecycleError::Protocol)?,
        revision: definition.revision,
        ttl_ticks: offer_ttl_ticks(),
        scope: Scope {
            workspace_key: workspace_key.to_string(),
            conversation_id: conversation_id.to_string(),
        },
        responder_policy: ResponderPolicy {
            audiences: vec![audience],
            // The terminal operator's authority comes from holding the
            // terminal, and the web decision channel presents no
            // credential today — so requiring one here would fail every
            // real answer. #1839 records the enrollment-aware policy this
            // should become.
            requires_assertion: false,
        },
        provenance: Provenance {
            origin: "permission-gate".to_string(),
            minted_tick,
        },
    };
    let lifecycle = publish(&HostMint::assert_host_authority(), &instance, definition)?;
    Ok((instance, lifecycle))
}

/// Express a decoded action as a response to `instance`.
fn response_for(
    definition: &InteractionDefinition,
    instance: &InteractionInstance,
    action: PermissionAction,
    audience: Audience,
) -> Result<Response, ProtocolError> {
    Ok(Response {
        schema: newt_interaction::ResponseTag,
        definition: definition.definition_id()?,
        instance: instance.instance_id()?,
        revision: definition.revision,
        values: vec![Submission {
            control: ControlId::new(DECISION_CONTROL)?,
            value: ControlValue::Choice {
                option: OptionId::new(action.as_str())?,
            },
        }],
        idempotency_key: IdempotencyKey::new(uuid::Uuid::new_v4().simple().to_string())?,
        responder_provenance: ResponderProvenance {
            kind: match audience {
                Audience::Terminal => AssertionKind::TerminalOperator,
                // Every other audience presents no credential today. Recorded
                // as unauthenticated rather than hidden.
                _ => AssertionKind::Unauthenticated,
            },
            subject: match audience {
                Audience::Terminal => "operator:terminal".to_string(),
                _ => "operator:web".to_string(),
            },
            audience,
            // No assertion is presented on either surface today. `None` is
            // the honest record; `requires_assertion: false` above is what
            // makes it admissible, and both are one decision, stated twice
            // rather than assumed once.
            assertion: None,
        },
    })
}

/// Authorize one decoded action against the offer it answers.
///
/// `registered` is the CALLER's registry of actions it can actually
/// execute. It is deliberately NOT derived from the definition: deriving
/// it would make [`Refusal::UnknownAction`] unfireable — every offered
/// option would be registered by construction — and would hand a
/// definition the power to name an executable thing (law 13).
///
/// # Errors
///
/// A [`Refusal`] when any binding fails. The caller denies; there is no
/// partial acceptance and no second opinion to consult.
pub fn authorize_action(
    definition: &InteractionDefinition,
    instance: &InteractionInstance,
    lifecycle: &Lifecycle,
    workspace_key: &str,
    registered: &[RegisteredAction],
    action: PermissionAction,
    audience: Audience,
) -> Result<PermissionAction, Refusal> {
    let response =
        response_for(definition, instance, action, audience.clone()).map_err(Refusal::Protocol)?;
    let accepted = newt_interaction::validate_response(
        definition,
        instance,
        lifecycle,
        &response,
        &ResponderContext {
            // The CALLER's fence, never the instance's own. Reading it off
            // the record being checked would make
            // `Refusal::WorkspaceMismatch` unfireable by construction —
            // a check that cannot fail is decoration.
            workspace_key,
            registered,
        },
    )?;
    // Exactly one control, so exactly one action — and it is the one the
    // definition offered under that option id, not the one the caller
    // asked about.
    let [resolved] = accepted.actions.as_slice() else {
        return Err(Refusal::MissingRequiredControl {
            control: DECISION_CONTROL.to_string(),
        });
    };
    crate::interaction_adapter::action_for_option(resolved.option.as_str()).ok_or_else(|| {
        Refusal::UnknownAction {
            option: resolved.option.as_str().to_string(),
        }
    })
}

/// The actions the permission gate can actually execute, and from which
/// audience.
///
/// **Independent of any definition, deliberately.** Deriving this from the
/// form would make [`Refusal::UnknownAction`] unfireable and would let a
/// definition name an executable thing (law 13). It is one registry shared
/// by both surfaces rather than a copy per surface, so the two cannot
/// drift.
///
/// The durable grants are terminal-only here, mirroring policy without
/// being derived from it — so a web submission naming `allow_permanent` is
/// refused as ineligible even if the form it was rendered from offered it.
#[must_use]
pub fn permission_registry(audience: Audience) -> Vec<RegisteredAction> {
    let terminal_only = matches!(audience, Audience::Terminal);
    let mut registry = Vec::new();
    for (action, handler, durable) in [
        (PermissionAction::AllowOnce, "gate::allow_once", false),
        (PermissionAction::AllowSession, "gate::allow_session", false),
        (
            PermissionAction::AllowPermanent,
            "gate::allow_permanent",
            true,
        ),
        (PermissionAction::Deny, "gate::deny", false),
        (PermissionAction::DenyAlways, "gate::deny_always", true),
        (
            PermissionAction::DenyPermanent,
            "gate::deny_permanent",
            true,
        ),
    ] {
        if durable && !terminal_only {
            continue;
        }
        let (Ok(option), Ok(handler)) = (
            OptionId::new(action.as_str()),
            newt_interaction::binding::HandlerId::new(handler),
        ) else {
            continue;
        };
        registry.push(RegisteredAction {
            option,
            handler,
            audiences: vec![audience.clone()],
        });
    }
    registry
}

/// Whether a web-submitted action is authorized by the published form.
///
/// **This is the web surface's half of the accept/deny move** (B0b-1,
/// #1842). It used to be `question.parse(action.as_str()) == Some(action)`
/// inside the store's answer transaction — a decode standing in for an
/// authorization. Now the stored form is read back as the definition it
/// is (through the A2.2 adapter, both directions of which are proven
/// field-identical) and the submission is validated against it.
///
/// Fails CLOSED on every error: an unreadable form, a form the adapter
/// cannot express, and any refusal all return `false`.
///
/// The bound this does NOT yet carry: the instance is minted here rather
/// than read back from the offer the gate published, because B0b-1 changes
/// no schema. So the DEFINITION binding is authoritative — an action the
/// published form does not carry is refused — while the INSTANCE binding
/// is self-consistent rather than proof the answer reached the offer that
/// was actually published. Persisting the instance is #1846.
#[must_use]
pub fn web_answer_is_authorized(
    question_json: &str,
    action: PermissionAction,
    workspace_key: &str,
    conversation_id: &str,
) -> bool {
    let Ok(question) = serde_json::from_str::<crate::Question<PermissionAction>>(question_json)
    else {
        return false;
    };
    let Ok(definition) = crate::interaction_adapter::question_to_definition(&question) else {
        return false;
    };
    let Ok((instance, lifecycle)) = mint_offer(
        &definition,
        workspace_key,
        conversation_id,
        Audience::Web,
        now_tick(),
    ) else {
        return false;
    };
    authorize_action(
        &definition,
        &instance,
        &lifecycle,
        workspace_key,
        &permission_registry(Audience::Web),
        action,
        Audience::Web,
    )
    .is_ok()
}
