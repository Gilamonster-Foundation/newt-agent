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

use crate::definition::{ChoiceOption, Control, ControlKind, InteractionDefinition, Requirement};
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
/// Resolve TYPED input to the option it names, or refuse.
///
/// **The one implementation of the canonical-first / alias / ambiguity-denial
/// rules.** It lived in `newt_core::tty::Question::parse`, over the legacy
/// `Action` list, and D0 (#1878) moved it here rather than writing a second
/// one against `ChoiceOption` — which is the third answer-parser this epic's
/// deletion gate exists to prevent.
///
/// It belongs beside [`check_value`] because the two are the same question
/// asked at different distances: `check_value` resolves a submission that
/// already names an `OptionId`, this resolves the text a human typed. Both
/// now live in one module, so "how is an answer resolved" has one answer.
///
/// ## The rules, unchanged
///
/// * **Canonical first.** An option's `id` (its wire name) or its `key` (the
///   accelerator) match before any alias is considered. The permission menu is
///   case-distinct on purpose — `a` is allow-once and `A` is allow-permanent —
///   so folding case, or letting an alias shadow a canonical key, would let a
///   weaker answer select a stronger grant.
/// * **Aliases second**, and only when nothing canonical matched.
/// * **Ambiguity refuses, at either tier.** Two matches is not a reason to
///   pick one.
///
/// ## Refusal is `None`, and what it MEANS is the caller's
///
/// This deliberately returns `Option`, not a default. A definition may come
/// from untrusted markup, and `ChoiceOption.role` is author-assigned — so
/// resolving an ambiguous answer by scanning the options for a `Deny` role
/// would let whoever wrote the definition choose the failure mode. The caller
/// supplies its own fail-closed constant; nothing here reads a role.
#[must_use]
pub fn resolve_typed(options: &[ChoiceOption], input: &str) -> Option<OptionId> {
    let input = input.trim();
    let single = |mut matches: std::vec::IntoIter<&ChoiceOption>| -> Option<OptionId> {
        let first = matches.next()?;
        // A second match is ambiguity, and ambiguity refuses.
        matches.next().is_none().then(|| first.id.clone())
    };

    let canonical: Vec<&ChoiceOption> = options
        .iter()
        .filter(|o| o.id.as_str() == input || o.key == input)
        .collect();
    if !canonical.is_empty() {
        return single(canonical.into_iter());
    }
    let aliased: Vec<&ChoiceOption> = options
        .iter()
        .filter(|o| o.aliases.iter().any(|alias| alias == input))
        .collect();
    single(aliased.into_iter())
}

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

#[cfg(test)]
mod resolve_typed_tests {
    use super::resolve_typed;
    use crate::definition::{ChoiceOption, SemanticRole};
    use crate::ids::OptionId;

    /// **These tests MOVED here with the rule they govern (D0, #1878).**
    ///
    /// They lived beside `newt_core::tty::Question::parse`, which was the one
    /// implementation of canonical-first / alias / ambiguity-denial until D0
    /// moved it into `resolve_typed` so a second decoder would not have to be
    /// written against `InteractionDefinition`. The coverage follows the code;
    /// deleting it and re-deriving it later is how a rule loses its tests.
    ///
    /// `BHV-PROMPT-001`'s and `BHV-PROMPT-005`'s refs were repointed in the
    /// same change.
    fn option(id: &str, role: SemanticRole, key: &str, aliases: &[&str]) -> ChoiceOption {
        ChoiceOption {
            id: OptionId::new(id).expect("valid id"),
            role,
            label: id.to_string(),
            key: key.to_string(),
            aliases: aliases.iter().map(|a| (*a).to_string()).collect(),
        }
    }

    /// The general permission menu: case-distinct `a`/`A` and `d`/`D`.
    fn menu() -> Vec<ChoiceOption> {
        vec![
            option("allow_once", SemanticRole::Allow, "a", &[]),
            option("allow_permanent", SemanticRole::Allow, "A", &[]),
            option("deny", SemanticRole::Deny, "d", &[]),
            option("deny_always", SemanticRole::Deny, "D", &[]),
        ]
    }

    /// The mutation-confirm form: y/Y confirm, n/N deny, via hidden aliases.
    fn confirm() -> Vec<ChoiceOption> {
        vec![
            option("allow_once", SemanticRole::Allow, "y", &["Y"]),
            option("deny", SemanticRole::Deny, "n", &["N"]),
        ]
    }

    fn resolved(options: &[ChoiceOption], input: &str) -> Option<String> {
        resolve_typed(options, input).map(|id| id.as_str().to_string())
    }

    #[test]
    fn lowercase_y_confirms() {
        assert_eq!(resolved(&confirm(), "y").as_deref(), Some("allow_once"));
    }

    #[test]
    fn uppercase_y_confirms_via_alias() {
        assert_eq!(resolved(&confirm(), "Y").as_deref(), Some("allow_once"));
    }

    #[test]
    fn lower_and_upper_n_deny() {
        assert_eq!(resolved(&confirm(), "n").as_deref(), Some("deny"));
        assert_eq!(resolved(&confirm(), "N").as_deref(), Some("deny"));
    }

    #[test]
    fn empty_unknown_and_malformed_input_resolve_to_nothing() {
        for bad in ["", "q", "yes", "y n", "\u{1b}", "yy"] {
            assert_eq!(
                resolved(&confirm(), bad),
                None,
                "input {bad:?} must not resolve"
            );
        }
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(resolved(&confirm(), "  Y  ").as_deref(), Some("allow_once"));
        assert_eq!(resolved(&confirm(), "\tn\n").as_deref(), Some("deny"));
    }

    /// Aliases must NOT fold case on the general menu: folding would let a
    /// weaker answer select a stronger grant.
    #[test]
    fn general_menu_parsing_stays_case_sensitive() {
        assert_eq!(resolved(&menu(), "a").as_deref(), Some("allow_once"));
        assert_eq!(resolved(&menu(), "A").as_deref(), Some("allow_permanent"));
        assert_eq!(resolved(&menu(), "d").as_deref(), Some("deny"));
        assert_eq!(resolved(&menu(), "D").as_deref(), Some("deny_always"));
    }

    /// The stable wire name resolves as well as the accelerator.
    #[test]
    fn value_wire_name_still_matches() {
        assert_eq!(
            resolved(&menu(), "allow_permanent").as_deref(),
            Some("allow_permanent")
        );
    }

    /// An alias cannot select an option the form does not offer.
    #[test]
    fn an_alias_cannot_select_an_action_absent_from_the_question() {
        let deny_only = vec![option("deny", SemanticRole::Deny, "n", &["N"])];
        assert_eq!(resolved(&deny_only, "Y"), None);
        assert_eq!(resolved(&deny_only, "y"), None);
    }

    /// **Ambiguity refuses, at BOTH tiers — never "first match wins".**
    #[test]
    fn ambiguity_refuses_rather_than_guessing() {
        // Two options claiming the same canonical key.
        let dup_key = vec![
            option("allow_once", SemanticRole::Allow, "x", &[]),
            option("deny", SemanticRole::Deny, "x", &[]),
        ];
        assert_eq!(resolved(&dup_key, "x"), None, "ambiguous key must refuse");

        // Two options claiming the same alias.
        let dup_alias = vec![
            option("allow_once", SemanticRole::Allow, "y", &["z"]),
            option("deny", SemanticRole::Deny, "n", &["z"]),
        ];
        assert_eq!(
            resolved(&dup_alias, "z"),
            None,
            "ambiguous alias must refuse"
        );
        // …and each is still individually resolvable, or the above passes
        // because nothing resolves at all.
        assert_eq!(resolved(&dup_alias, "y").as_deref(), Some("allow_once"));
        assert_eq!(resolved(&dup_alias, "n").as_deref(), Some("deny"));
    }

    /// A canonical match wins over an alias, so an alias can never shadow
    /// another option's real key or wire name.
    #[test]
    fn a_canonical_match_beats_an_alias() {
        let shadowing = vec![
            option("allow_once", SemanticRole::Allow, "a", &[]),
            // `deny` claims "a" as an ALIAS — it must not win.
            option("deny", SemanticRole::Deny, "d", &["a"]),
        ];
        assert_eq!(resolved(&shadowing, "a").as_deref(), Some("allow_once"));
    }

    /// An alias must not shadow another option's WIRE NAME either — canonical
    /// covers both the key and the id, and it is checked first.
    #[test]
    fn an_alias_never_shadows_another_options_wire_name() {
        let shadowing = vec![
            option("deny", SemanticRole::Deny, "d", &[]),
            // `allow_once` claims "deny" — the other option's wire name — as
            // an alias. Canonical wins.
            option("allow_once", SemanticRole::Allow, "a", &["deny"]),
        ];
        assert_eq!(resolved(&shadowing, "deny").as_deref(), Some("deny"));
    }

    /// A KEY on one option colliding with the WIRE NAME of another is
    /// ambiguous at the canonical tier, and refuses.
    #[test]
    fn key_value_collision_between_options_fails_closed() {
        let collide = vec![
            // this option's KEY is the other's wire name
            option("allow_once", SemanticRole::Allow, "deny", &[]),
            option("deny", SemanticRole::Deny, "d", &[]),
        ];
        assert_eq!(
            resolved(&collide, "deny"),
            None,
            "a key-vs-wire-name collision is ambiguous and must refuse"
        );
    }

    /// **Refusal is `None`, and nothing here reads a role to decide what
    /// refusal means.** A definition can come from untrusted markup and
    /// `role` is author-assigned, so a resolver that fell back to the `Deny`
    /// option would let the author choose the failure mode (A3). Both these
    /// forms refuse identically despite opposite role layouts.
    #[test]
    fn refusal_does_not_consult_the_authored_role() {
        let deny_first = vec![
            option("deny", SemanticRole::Deny, "d", &[]),
            option("allow_once", SemanticRole::Allow, "a", &[]),
        ];
        let allow_first = vec![
            option("allow_once", SemanticRole::Allow, "a", &[]),
            option("deny", SemanticRole::Deny, "d", &[]),
        ];
        // An author who marks EVERYTHING Allow gets no say either.
        let all_allow = vec![
            option("allow_once", SemanticRole::Allow, "a", &[]),
            option("deny", SemanticRole::Allow, "d", &[]),
        ];
        for options in [&deny_first, &allow_first, &all_allow] {
            assert_eq!(
                resolved(options, "nonsense"),
                None,
                "an unresolvable answer yields NOTHING, not a role-chosen option"
            );
        }
    }
}
