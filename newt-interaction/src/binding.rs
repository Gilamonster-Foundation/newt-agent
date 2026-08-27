//! **Fail-closed response validation** (A3, #1837).
//!
//! Pure rules over types this crate already owns — the same shape as
//! [`plan_presentation`](crate::plan_presentation), and for the same
//! reason: none of it touches SQLite, a TTY, or a socket, so none of it
//! belongs in a crate that does. Keeping the security-relevant logic in
//! the dependency-minimal layer is what makes it auditable.
//!
//! **Validation is ATOMIC.** A response is accepted as a whole or refused
//! as a whole. There is no partial acceptance and no "apply the controls
//! that checked out" path, because a caller handed half an answer would
//! have to decide what the other half meant — which is exactly the
//! per-surface drift this epic exists to end. It is also what keeps the
//! Lean `authorize` / `hidden_action_rejected` generalization honest: a
//! hidden or unoffered control cannot ride along beside a valid one.
//!
//! **Actions map only to caller-registered handlers.** Nothing in a
//! definition is executable. A definition carries markdown, labels, and
//! option ids authored by — possibly — untrusted markup, and this module
//! never turns any of it into a command, a URL, a tool name, a path, a
//! topic, or a caveat. The accepted outcome names a
//! [`HandlerId`](crate::binding::HandlerId) the CALLER registered, and
//! nothing else. A registry owned down here would be a second capability
//! system growing beside the store (epic law 13); authority-minting stays
//! above this layer.

use thiserror::Error;

use crate::definition::{Control, ControlKind, InteractionDefinition, Requirement};
use crate::error::ProtocolError;
use crate::ids::{ControlId, InstanceId, OptionId, ResponseId};
use crate::instance::{Audience, InteractionInstance, LifecycleState};
use crate::lifecycle::Lifecycle;
use crate::response::{AssertionKind, ControlValue, Response};

/// A caller-supplied handler name.
///
/// Opaque to this crate: it routes to something the CALLER holds. This
/// layer never dereferences it, never interprets it, and never derives
/// one from a definition.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HandlerId(String);

impl HandlerId {
    /// Adopt a handler name.
    ///
    /// # Errors
    ///
    /// [`ProtocolError::InvalidId`] when empty — a handler that names
    /// nothing cannot be routed to.
    pub fn new(name: impl Into<String>) -> Result<Self, ProtocolError> {
        let name = name.into();
        if name.is_empty() {
            return Err(ProtocolError::InvalidId {
                kind: "handler id",
                reason: "must not be empty".to_string(),
            });
        }
        Ok(Self(name))
    }

    /// The name as adopted.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One action the CALLER has registered, and who may invoke it.
///
/// The caller injects these. A definition cannot add one, which is the
/// property that keeps authored markup from naming an executable thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredAction {
    /// The option this action answers to.
    pub option: OptionId,
    /// What the caller will run.
    pub handler: HandlerId,
    /// Which audiences may invoke it. An audience the offer admits is not
    /// automatically an audience every ACTION admits.
    pub audiences: Vec<Audience>,
}

/// What the caller brings to a validation: its fence and its handlers.
#[derive(Debug, Clone, Copy)]
pub struct ResponderContext<'a> {
    /// The workspace fence the caller is operating in.
    pub workspace_key: &'a str,
    /// The actions the caller has registered.
    pub registered: &'a [RegisteredAction],
}

/// One accepted submission, resolved to the handler the caller registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAction {
    /// Which control was answered.
    pub control: ControlId,
    /// Which option was chosen.
    pub option: OptionId,
    /// The CALLER's handler. Never derived from the definition.
    pub handler: HandlerId,
}

/// A response that passed every rule, with its actions resolved.
///
/// Carries ids and caller-registered handler names only. Deliberately no
/// markdown, label, note, or any other author-supplied string: an
/// outcome that carried document text would invite a caller to act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    /// The response that was accepted.
    pub response: ResponseId,
    /// The offer it resolves.
    pub instance: InstanceId,
    /// The actions to run, in submission order.
    pub actions: Vec<ResolvedAction>,
}

/// Why a response was refused. Every variant is a REFUSAL: there is no
/// variant meaning "accepted with reservations".
#[derive(Debug, Error)]
pub enum Refusal {
    /// The offer is not open for answers.
    #[error("the offer is {state:?}, not Published; it accepts no response")]
    NotPublished {
        /// The state the offer is actually in.
        state: LifecycleState,
    },
    /// The response answers a different offer.
    #[error("the response answers instance `{named}`, but this offer is `{actual}`")]
    InstanceMismatch {
        /// What the response named.
        named: String,
        /// The offer being resolved.
        actual: String,
    },
    /// The response answers a different definition than the offer binds.
    #[error("the response answers definition `{named}`, but the offer binds `{offered}`")]
    DefinitionMismatch {
        /// What the response named.
        named: String,
        /// What the offer binds.
        offered: String,
    },
    /// The definition presented is not the one the response was minted
    /// against — it changed by at least one byte.
    #[error("definition digest mismatch: the response binds `{bound}`, the presented form is `{presented}`")]
    DigestMismatch {
        /// The digest the response binds.
        bound: String,
        /// The digest of the definition actually presented.
        presented: String,
    },
    /// The responder answered an older revision.
    #[error("the response is against revision {offered}, but the definition is at {current}")]
    StaleRevision {
        /// The revision the responder saw.
        offered: u64,
        /// The revision the definition is at now.
        current: u64,
    },
    /// The offer belongs to a different workspace fence.
    #[error(
        "workspace fence mismatch: the offer is fenced to `{offered}`, the caller is in `{caller}`"
    )]
    WorkspaceMismatch {
        /// The fence on the offer.
        offered: String,
        /// The caller's fence.
        caller: String,
    },
    /// The responder answered from an audience the offer is not open to.
    #[error("this offer is not open to the {audience:?} audience")]
    AudienceMismatch {
        /// The audience that answered.
        audience: Audience,
    },
    /// The offer requires an authenticated assertion and none was
    /// presented.
    #[error("this offer requires an authenticated assertion")]
    AssertionRequired,
    /// The response answers a control the definition does not offer.
    #[error("`{control}` is not a control of this definition")]
    ExtraControl {
        /// The control the response named.
        control: String,
    },
    /// The response answers one control twice.
    #[error("`{control}` is answered more than once")]
    DuplicateControl {
        /// The control answered twice.
        control: String,
    },
    /// A REQUIRED control has no answer.
    #[error("required control `{control}` has no answer")]
    MissingRequiredControl {
        /// The unanswered control.
        control: String,
    },
    /// The value's kind does not match the control's kind.
    #[error("control `{control}` is {expected}, but the value is {found}")]
    WrongControlType {
        /// The control answered.
        control: String,
        /// The kind the definition declares.
        expected: &'static str,
        /// The kind the value carries.
        found: &'static str,
    },
    /// The chosen option is not one the control offers.
    #[error("control `{control}` does not offer option `{option}`")]
    UnknownOption {
        /// The control answered.
        control: String,
        /// The option chosen.
        option: String,
    },
    /// The chosen option maps to no caller-registered handler.
    #[error("option `{option}` maps to no registered handler")]
    UnknownAction {
        /// The option chosen.
        option: String,
    },
    /// The action exists but this responder may not invoke it.
    #[error("option `{option}` is not eligible for the {audience:?} audience")]
    ActionNotEligible {
        /// The option chosen.
        option: String,
        /// The audience that answered.
        audience: Audience,
    },
    /// Content addressing failed while checking a binding.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// The name of a control's kind, for a refusal message.
fn kind_name(kind: &ControlKind) -> &'static str {
    match kind {
        ControlKind::Choice { .. } => "choice",
        ControlKind::Text => "text",
        ControlKind::Toggle => "toggle",
        ControlKind::Secret => "secret",
    }
}

/// The name of a value's kind, for a refusal message.
fn value_name(value: &ControlValue) -> &'static str {
    match value {
        ControlValue::Choice { .. } => "choice",
        ControlValue::Text { .. } => "text",
        ControlValue::Toggle { .. } => "toggle",
        ControlValue::Secret { .. } => "secret",
    }
}

/// Check one submission against the control it claims to answer.
///
/// Returns the chosen option when the control is a choice, so the caller
/// can resolve it to a handler; `None` for the value-carrying kinds,
/// which route to nothing.
fn check_value(control: &Control, value: &ControlValue) -> Result<Option<OptionId>, Refusal> {
    match (&control.kind, value) {
        (ControlKind::Choice { options }, ControlValue::Choice { option }) => {
            if options.iter().any(|o| &o.id == option) {
                Ok(Some(option.clone()))
            } else {
                Err(Refusal::UnknownOption {
                    control: control.id.as_str().to_string(),
                    option: option.as_str().to_string(),
                })
            }
        }
        (ControlKind::Text, ControlValue::Text { .. })
        | (ControlKind::Toggle, ControlValue::Toggle { .. })
        | (ControlKind::Secret, ControlValue::Secret { .. }) => Ok(None),
        _ => Err(Refusal::WrongControlType {
            control: control.id.as_str().to_string(),
            expected: kind_name(&control.kind),
            found: value_name(value),
        }),
    }
}

/// Whether the responder presented an assertion that satisfies the offer.
fn assertion_satisfied(response: &Response) -> bool {
    match response.responder_provenance.kind {
        AssertionKind::Unauthenticated => false,
        AssertionKind::TerminalOperator | AssertionKind::SignedAssertion => {
            response.responder_provenance.assertion.is_some()
        }
    }
}

/// Validate `response` against the offer it claims to answer.
///
/// Every rule is a refusal, and the whole `Vec<Submission>` stands or
/// falls together: this returns [`Accepted`] with every action resolved,
/// or a [`Refusal`] and nothing at all.
///
/// # Errors
///
/// A [`Refusal`] naming the first rule the response broke. No partial
/// result is produced for a response that breaks any rule.
pub fn validate_response(
    definition: &InteractionDefinition,
    instance: &InteractionInstance,
    lifecycle: &Lifecycle,
    response: &Response,
    context: &ResponderContext<'_>,
) -> Result<Accepted, Refusal> {
    // 1. The offer must be open. An expired, cancelled, answered, or
    //    unsupported offer refuses EVERY response, valid or not.
    if lifecycle.state() != LifecycleState::Published {
        return Err(Refusal::NotPublished {
            state: lifecycle.state(),
        });
    }

    // 2. The response must answer THIS offer.
    let instance_id = instance.instance_id().map_err(Refusal::Protocol)?;
    if response.instance != instance_id {
        return Err(Refusal::InstanceMismatch {
            named: response.instance.to_string(),
            actual: instance_id.to_string(),
        });
    }
    if lifecycle.instance() != &instance_id {
        return Err(Refusal::InstanceMismatch {
            named: lifecycle.instance().to_string(),
            actual: instance_id.to_string(),
        });
    }

    // 3. ...and the definition that offer binds...
    if response.definition != instance.definition {
        return Err(Refusal::DefinitionMismatch {
            named: response.definition.to_string(),
            offered: instance.definition.to_string(),
        });
    }

    // 4. ...in the exact form it was minted against. A definition that
    //    changed by one byte has a different id, so a response minted
    //    against the old one cannot answer the new one.
    let presented = definition.definition_id().map_err(Refusal::Protocol)?;
    if response.definition != presented {
        return Err(Refusal::DigestMismatch {
            bound: response.definition.to_string(),
            presented: presented.to_string(),
        });
    }

    // 5. ...at the revision the responder actually saw.
    if response.revision != definition.revision {
        return Err(Refusal::StaleRevision {
            offered: response.revision.get(),
            current: definition.revision.get(),
        });
    }

    // 6. The workspace fence.
    if instance.scope.workspace_key != context.workspace_key {
        return Err(Refusal::WorkspaceMismatch {
            offered: instance.scope.workspace_key.clone(),
            caller: context.workspace_key.to_string(),
        });
    }

    // 7. Responder policy: audience, then assertion.
    let audience = response.responder_provenance.audience.clone();
    if !instance.responder_policy.audiences.contains(&audience) {
        return Err(Refusal::AudienceMismatch { audience });
    }
    if instance.responder_policy.requires_assertion && !assertion_satisfied(response) {
        return Err(Refusal::AssertionRequired);
    }

    // 8. The control set, exactly: no extras, no duplicates, every
    //    required control answered, every value of the declared type.
    let mut answered: Vec<&ControlId> = Vec::new();
    let mut actions = Vec::new();
    for submission in &response.values {
        let Some(control) = definition
            .controls
            .iter()
            .find(|c| c.id == submission.control)
        else {
            return Err(Refusal::ExtraControl {
                control: submission.control.as_str().to_string(),
            });
        };
        if answered.contains(&&submission.control) {
            return Err(Refusal::DuplicateControl {
                control: submission.control.as_str().to_string(),
            });
        }
        answered.push(&submission.control);

        if let Some(option) = check_value(control, &submission.value)? {
            // 9. An action routes ONLY to a handler the caller
            //    registered, and only for an eligible audience.
            let Some(registration) = context.registered.iter().find(|r| r.option == option) else {
                return Err(Refusal::UnknownAction {
                    option: option.as_str().to_string(),
                });
            };
            if !registration.audiences.contains(&audience) {
                return Err(Refusal::ActionNotEligible {
                    option: option.as_str().to_string(),
                    audience,
                });
            }
            actions.push(ResolvedAction {
                control: control.id.clone(),
                option,
                handler: registration.handler.clone(),
            });
        }
    }

    for control in &definition.controls {
        if control.requirement == Requirement::Required && !answered.contains(&&control.id) {
            return Err(Refusal::MissingRequiredControl {
                control: control.id.as_str().to_string(),
            });
        }
    }

    Ok(Accepted {
        response: response.response_id().map_err(Refusal::Protocol)?,
        instance: instance_id,
        actions,
    })
}

#[cfg(test)]
mod model_fidelity {
    use super::*;

    /// **A third `Audience` variant would silently invalidate the formal
    /// model, so it must not compile silently.**
    ///
    /// `PromptControls.tla` models two racers and a BINARY `SingleWinner`;
    /// `Audience` is currently exactly `{Terminal, Web}`. It is
    /// `#[non_exhaustive]`, so no downstream crate can write an
    /// exhaustive match over it and no downstream test can notice a third
    /// variant. This crate DEFINES it, so the match below — deliberately
    /// without a `_` arm — is the one place that can: adding a variant
    /// breaks compilation here, which is the prompt to revisit the model
    /// rather than discover the gap later.
    #[test]
    fn the_modelled_audience_set_is_exactly_the_two_the_tla_covers() {
        let modelled = [Audience::Terminal, Audience::Web];
        for audience in &modelled {
            match audience {
                Audience::Terminal | Audience::Web => {}
            }
        }
        assert_eq!(
            modelled.len(),
            2,
            "PromptControls.tla's SingleWinner is binary; a third audience \
             needs the model revisited, not just this number bumped"
        );
    }
}
