//! **A3 controller: lifecycle, fail-closed binding, exactly-once
//! resolution** (#1837).
//!
//! Every test here is pure. The persistence contract is exercised through
//! an in-memory [`ResolutionStore`] defined at the bottom of this file —
//! deliberately NOT a second production implementation, and deliberately
//! not SQL-shaped: the contract does not mandate a SQL shape, and an
//! in-memory implementation collapsing to one compare-and-swap is a
//! legitimate reading of it. The cross-connection race that only SQLite
//! can exhibit is tested where SQLite lives, in
//! `newt-core/tests/interaction_resolution.rs`.

use std::collections::BTreeMap;
use std::sync::Mutex;

use newt_interaction::binding::{
    validate_response, Accepted, HandlerId, Refusal, RegisteredAction, ResolvedAction,
    ResponderContext,
};
use newt_interaction::lifecycle::{publish, HostMint, Lifecycle, LifecycleError};
use newt_interaction::resolution::{
    Resolution, ResolutionError, ResolutionRecord, ResolutionStore,
};
use newt_interaction::{
    AssertionKind, Audience, ChoiceOption, Control, ControlId, ControlKind, ControlValue,
    IdempotencyKey, InstanceId, InteractionDefinition, InteractionInstance, InteractionKind,
    LifecycleState, Nonce, OptionId, Provenance, ResponderPolicy, ResponderProvenance, Response,
    ResponseId, Revision, Scope, SemanticRole, Submission,
};

const WORKSPACE: &str = "ws-a3";

fn option(id: &str, role: SemanticRole, label: &str) -> ChoiceOption {
    ChoiceOption {
        id: OptionId::new(id).unwrap(),
        role,
        label: label.to_string(),
        key: String::new(),
        aliases: Vec::new(),
    }
}

/// A choice definition with an allow and a deny, plus an optional note
/// control so "required vs optional" is exercisable.
fn definition() -> InteractionDefinition {
    InteractionDefinition::new(
        InteractionKind::Choice,
        "⊘ run_command wants to run `bash`",
        vec![
            Control {
                id: ControlId::new("decision").unwrap(),
                kind: ControlKind::Choice {
                    options: vec![
                        option("allow-once", SemanticRole::Allow, "allow once"),
                        option("deny", SemanticRole::Deny, "deny (default)"),
                    ],
                },
                label: "what should happen".to_string(),
                requirement: newt_interaction::Requirement::Required,
            },
            Control {
                id: ControlId::new("reason").unwrap(),
                kind: ControlKind::Text,
                label: "why".to_string(),
                requirement: newt_interaction::Requirement::Optional,
            },
        ],
    )
}

fn instance_for(def: &InteractionDefinition) -> InteractionInstance {
    InteractionInstance {
        schema: newt_interaction::InstanceTag,
        nonce: Nonce::new("1756200000000000000-0f4c1b2e").unwrap(),
        definition: def.definition_id().unwrap(),
        revision: def.revision,
        ttl_ticks: 300,
        scope: Scope {
            workspace_key: WORKSPACE.to_string(),
            conversation_id: "conv-1".to_string(),
        },
        responder_policy: ResponderPolicy {
            audiences: vec![Audience::Terminal],
            requires_assertion: false,
        },
        provenance: Provenance {
            origin: "permission-gate".to_string(),
            minted_tick: 1_000,
        },
    }
}

fn response_for(
    def: &InteractionDefinition,
    inst: &InteractionInstance,
    option_id: &str,
    key: &str,
) -> Response {
    Response {
        schema: newt_interaction::ResponseTag,
        definition: def.definition_id().unwrap(),
        instance: inst.instance_id().unwrap(),
        revision: def.revision,
        values: vec![Submission {
            control: ControlId::new("decision").unwrap(),
            value: ControlValue::Choice {
                option: OptionId::new(option_id).unwrap(),
            },
        }],
        idempotency_key: IdempotencyKey::new(key).unwrap(),
        responder_provenance: ResponderProvenance {
            kind: AssertionKind::TerminalOperator,
            subject: "operator:hartsock".to_string(),
            audience: Audience::Terminal,
            assertion: Some("tty-1".to_string()),
        },
    }
}

fn registered() -> Vec<RegisteredAction> {
    vec![
        RegisteredAction {
            option: OptionId::new("allow-once").unwrap(),
            handler: HandlerId::new("gate::allow_once").unwrap(),
            audiences: vec![Audience::Terminal],
        },
        RegisteredAction {
            option: OptionId::new("deny").unwrap(),
            handler: HandlerId::new("gate::deny").unwrap(),
            audiences: vec![Audience::Terminal, Audience::Web],
        },
    ]
}

fn context<'a>(registered: &'a [RegisteredAction]) -> ResponderContext<'a> {
    ResponderContext {
        workspace_key: WORKSPACE,
        registered,
    }
}

fn published(def: &InteractionDefinition, inst: &InteractionInstance) -> Lifecycle {
    publish(&HostMint::assert_host_authority(), inst, def).expect("publishes")
}

mod lifecycle {
    use super::*;

    /// Publication verifies the binding the offer claims. An instance
    /// naming a definition the host never presented is refused rather
    /// than believed — the runtime half of "host-minted".
    #[test]
    fn only_a_host_minted_instance_can_be_published() {
        let def = definition();
        let inst = instance_for(&def);
        assert!(publish(&HostMint::assert_host_authority(), &inst, &def).is_ok());

        // An offer forged against a DIFFERENT definition than the host
        // holds.
        let other = InteractionDefinition::new(InteractionKind::Confirm, "something else", vec![]);
        let err = publish(&HostMint::assert_host_authority(), &inst, &other)
            .expect_err("a mismatched binding must not publish");
        assert!(
            matches!(err, LifecycleError::DefinitionMismatch { .. }),
            "expected DefinitionMismatch, got {err:?}"
        );

        // ...and one that binds a revision the definition is not at.
        let mut stale = instance_for(&def);
        stale.revision = Revision::new(7);
        let err = publish(&HostMint::assert_host_authority(), &stale, &def)
            .expect_err("a revision mismatch must not publish");
        assert!(
            matches!(err, LifecycleError::RevisionMismatch { .. }),
            "expected RevisionMismatch, got {err:?}"
        );
    }

    /// Law 11. Everything an untrusted document can supply is DATA: a
    /// definition, and even a well-formed instance record. Neither is a
    /// [`HostMint`], and no decode produces one — `HostMint` has a
    /// private field and implements neither `Deserialize` nor `Default`,
    /// so the only door is host code calling
    /// `assert_host_authority()`.
    #[test]
    fn authored_markup_cannot_publish_an_instance() {
        let authored = definition();
        // The document also supplies an instance record, minted against
        // its own definition, claiming a trusted origin. It decodes
        // fine — records are data.
        let mut forged = instance_for(&authored);
        forged.provenance.origin = "permission-gate".to_string();
        let bytes = {
            use content_addressable::{canonical, ContentAddressable};
            let _ = forged.content_id().unwrap();
            canonical::to_canonical_dagcbor(&forged).unwrap()
        };
        let decoded = newt_interaction::decode_instance(&bytes).expect("a record is just data");
        assert!(matches!(decoded, newt_interaction::Decoded::Known(_)));

        // What the document CANNOT supply is the authority. Publication
        // takes a `&HostMint` by type, so there is no value a decoder
        // can return that reaches this call — the host must vouch. The
        // host vouching for the host's OWN definition is the only path
        // that publishes.
        let host_definition = definition();
        assert!(publish(
            &HostMint::assert_host_authority(),
            &instance_for(&host_definition),
            &host_definition
        )
        .is_ok());
    }

    /// Answered, Cancelled, Expired and Unsupported accept nothing
    /// further — including a move to themselves.
    #[test]
    fn each_terminal_state_is_terminal() {
        let def = definition();
        let inst = instance_for(&def);
        let live = published(&def, &inst);

        for terminal in [
            LifecycleState::Answered,
            LifecycleState::Cancelled,
            LifecycleState::Expired,
            LifecycleState::Unsupported,
        ] {
            let resolved = live.transition(terminal).expect("published resolves");
            assert!(resolved.is_terminal(), "{terminal:?} must be terminal");
            for next in [
                LifecycleState::Answered,
                LifecycleState::Cancelled,
                LifecycleState::Expired,
                LifecycleState::Unsupported,
                LifecycleState::Published,
                LifecycleState::Draft,
            ] {
                let err = resolved
                    .transition(next)
                    .expect_err("a terminal state accepts nothing");
                assert!(
                    matches!(err, LifecycleError::AlreadyTerminal { .. }),
                    "{terminal:?} -> {next:?} gave {err:?}"
                );
            }
        }
    }

    /// An expired offer refuses every response — including one that is
    /// valid in every other respect.
    #[test]
    fn an_expired_instance_refuses_every_response() {
        let def = definition();
        let inst = instance_for(&def);
        let expired = published(&def, &inst).expire().expect("expires");
        let actions = registered();

        for option_id in ["allow-once", "deny"] {
            let response = response_for(&def, &inst, option_id, "k");
            let refusal = validate_response(&def, &inst, &expired, &response, &context(&actions))
                .expect_err("an expired offer accepts nothing");
            assert!(
                matches!(
                    refusal,
                    Refusal::NotPublished {
                        state: LifecycleState::Expired
                    }
                ),
                "expected NotPublished(Expired), got {refusal:?}"
            );
        }
    }

    /// The Rust counterpart of TLA `TimeoutNeverAuthorizes`. Expiry is a
    /// pure no-decision transition: it yields a state and an id, and
    /// there is no API on the result through which a `Response` — or any
    /// authorization — could arrive.
    #[test]
    fn expiry_synthesizes_no_response_and_never_authorizes() {
        let def = definition();
        let inst = instance_for(&def);
        let live = published(&def, &inst);

        assert!(!Lifecycle::has_elapsed(&inst, 1_299));
        assert!(Lifecycle::has_elapsed(&inst, 1_300));

        let expired = live.expire().expect("expires");
        assert_eq!(expired.state(), LifecycleState::Expired);
        assert_eq!(expired.instance(), live.instance());

        // The only thing expiry produced is a state. Nothing here can be
        // read as a decision, and every response — including one naming
        // the ALLOW option — is refused after it.
        let allow = response_for(&def, &inst, "allow-once", "k");
        let actions = registered();
        let refusal = validate_response(&def, &inst, &expired, &allow, &context(&actions))
            .expect_err("expiry must not authorize");
        assert!(matches!(refusal, Refusal::NotPublished { .. }));
    }

    /// `SemanticRole` is author-assigned, so a definition whose roles are
    /// deliberately mislabelled must produce the SAME expiry outcome as
    /// an honest one. If expiry consulted `role` at all, these two would
    /// diverge.
    #[test]
    fn a_mislabelled_option_role_cannot_become_the_expiry_default() {
        // Honest: "deny" is Deny.
        let honest = definition();
        // Mislabelled: the option LABELLED deny carries role Allow, and
        // the allow option carries Deny. An expiry default computed by
        // scanning for `role == Deny` would pick the option a document
        // author chose.
        let mislabelled = InteractionDefinition::new(
            InteractionKind::Choice,
            "⊘ run_command wants to run `bash`",
            vec![Control {
                id: ControlId::new("decision").unwrap(),
                kind: ControlKind::Choice {
                    options: vec![
                        option("allow-once", SemanticRole::Deny, "allow once"),
                        option("deny", SemanticRole::Allow, "deny (default)"),
                    ],
                },
                label: "what should happen".to_string(),
                requirement: newt_interaction::Requirement::Required,
            }],
        );

        let mut outcomes = Vec::new();
        for def in [&honest, &mislabelled] {
            let inst = instance_for(def);
            let expired = published(def, &inst).expire().expect("expires");
            outcomes.push(expired.state());

            // No response exists to be found, whatever the roles say.
            let actions = registered();
            for option_id in ["allow-once", "deny"] {
                let response = response_for(def, &inst, option_id, "k");
                assert!(
                    validate_response(def, &inst, &expired, &response, &context(&actions)).is_err(),
                    "expiry authorized something in a mislabelled definition"
                );
            }
        }
        assert_eq!(
            outcomes[0], outcomes[1],
            "the expiry outcome depends on author-assigned roles"
        );
    }
}

mod binding {
    use super::*;

    fn refuse(response: &Response) -> Refusal {
        let def = definition();
        let inst = instance_for(&def);
        let live = published(&def, &inst);
        let actions = registered();
        validate_response(&def, &inst, live_ref(&live), response, &context(&actions))
            .expect_err("expected a refusal")
    }

    fn live_ref(l: &Lifecycle) -> &Lifecycle {
        l
    }

    #[test]
    fn a_valid_response_is_accepted() {
        let def = definition();
        let inst = instance_for(&def);
        let live = published(&def, &inst);
        let actions = registered();
        let response = response_for(&def, &inst, "deny", "k");
        let accepted = validate_response(&def, &inst, &live, &response, &context(&actions))
            .expect("a valid response is accepted");
        assert_eq!(
            accepted.actions,
            vec![ResolvedAction {
                control: ControlId::new("decision").unwrap(),
                option: OptionId::new("deny").unwrap(),
                handler: HandlerId::new("gate::deny").unwrap(),
            }]
        );
    }

    #[test]
    fn a_response_to_a_stale_revision_is_refused() {
        let def = definition();
        let inst = instance_for(&def);
        let mut response = response_for(&def, &inst, "deny", "k");
        response.revision = Revision::new(9);
        assert!(
            matches!(refuse(&response), Refusal::StaleRevision { .. }),
            "a stale revision was accepted"
        );
    }

    #[test]
    fn a_response_naming_another_definition_is_refused() {
        let def = definition();
        let inst = instance_for(&def);
        let other = InteractionDefinition::new(InteractionKind::Confirm, "another", vec![]);
        let mut response = response_for(&def, &inst, "deny", "k");
        response.definition = other.definition_id().unwrap();
        assert!(
            matches!(refuse(&response), Refusal::DefinitionMismatch { .. }),
            "a response naming another definition was accepted"
        );
    }

    #[test]
    fn a_response_naming_another_instance_is_refused() {
        let def = definition();
        let inst = instance_for(&def);
        let mut other = instance_for(&def);
        other.nonce = Nonce::new("1756200000000000001-aaaaaaaa").unwrap();
        let mut response = response_for(&def, &inst, "deny", "k");
        response.instance = other.instance_id().unwrap();
        assert!(
            matches!(refuse(&response), Refusal::InstanceMismatch { .. }),
            "a response naming another instance was accepted"
        );
    }

    /// The offer and the response agree on the definition ID, but the
    /// definition the host PRESENTS has changed — so the form the
    /// responder answered no longer exists.
    #[test]
    fn a_digest_mismatch_is_refused() {
        let answered = definition();
        let inst = instance_for(&answered);
        let live = published(&answered, &inst);
        let response = response_for(&answered, &inst, "deny", "k");

        // One byte of markdown different: a different definition, a
        // different id.
        let mut mutated = definition();
        mutated.markdown.push('!');
        assert_ne!(
            mutated.definition_id().unwrap(),
            answered.definition_id().unwrap()
        );

        let actions = registered();
        let refusal = validate_response(&mutated, &inst, &live, &response, &context(&actions))
            .expect_err("a mutated definition must not accept the old response");
        assert!(
            matches!(refusal, Refusal::DigestMismatch { .. }),
            "expected DigestMismatch, got {refusal:?}"
        );
    }

    #[test]
    fn an_extra_control_is_refused() {
        let def = definition();
        let inst = instance_for(&def);
        let mut response = response_for(&def, &inst, "deny", "k");
        response.values.push(Submission {
            control: ControlId::new("not-a-control").unwrap(),
            value: ControlValue::Toggle { on: true },
        });
        assert!(
            matches!(refuse(&response), Refusal::ExtraControl { .. }),
            "an unoffered control rode along"
        );
    }

    #[test]
    fn a_missing_required_control_is_refused() {
        let def = definition();
        let inst = instance_for(&def);
        let mut response = response_for(&def, &inst, "deny", "k");
        response.values.clear();
        assert!(
            matches!(refuse(&response), Refusal::MissingRequiredControl { .. }),
            "a required control was allowed to go unanswered"
        );
    }

    #[test]
    fn a_wrong_control_type_is_refused() {
        let def = definition();
        let inst = instance_for(&def);
        let mut response = response_for(&def, &inst, "deny", "k");
        response.values[0].value = ControlValue::Toggle { on: true };
        assert!(
            matches!(refuse(&response), Refusal::WrongControlType { .. }),
            "a toggle answered a choice"
        );
    }

    #[test]
    fn an_unknown_action_is_refused() {
        let def = definition();
        let inst = instance_for(&def);
        let live = published(&def, &inst);
        let response = response_for(&def, &inst, "allow-once", "k");
        // The option IS offered by the definition, but the caller
        // registered no handler for it.
        let only_deny = vec![RegisteredAction {
            option: OptionId::new("deny").unwrap(),
            handler: HandlerId::new("gate::deny").unwrap(),
            audiences: vec![Audience::Terminal],
        }];
        let refusal = validate_response(&def, &inst, &live, &response, &context(&only_deny))
            .expect_err("an unregistered action must not run");
        assert!(
            matches!(refusal, Refusal::UnknownAction { .. }),
            "expected UnknownAction, got {refusal:?}"
        );

        // ...and an option the DEFINITION does not offer is refused
        // before it ever reaches the registry.
        let mut forged = response_for(&def, &inst, "deny", "k");
        forged.values[0].value = ControlValue::Choice {
            option: OptionId::new("escalate").unwrap(),
        };
        assert!(matches!(refuse(&forged), Refusal::UnknownOption { .. }));
    }

    #[test]
    fn an_action_not_eligible_for_this_responder_is_refused() {
        let def = definition();
        let mut inst = instance_for(&def);
        // The OFFER admits web, but the allow-once ACTION does not.
        inst.responder_policy.audiences = vec![Audience::Terminal, Audience::Web];
        let live = published(&def, &inst);
        let mut response = response_for(&def, &inst, "allow-once", "k");
        response.responder_provenance.audience = Audience::Web;

        let actions = registered();
        let refusal = validate_response(&def, &inst, &live, &response, &context(&actions))
            .expect_err("an ineligible action must not run");
        assert!(
            matches!(refusal, Refusal::ActionNotEligible { .. }),
            "expected ActionNotEligible, got {refusal:?}"
        );
    }

    #[test]
    fn an_audience_mismatch_is_refused() {
        let def = definition();
        let inst = instance_for(&def);
        let mut response = response_for(&def, &inst, "deny", "k");
        response.responder_provenance.audience = Audience::Web;
        assert!(
            matches!(refuse(&response), Refusal::AudienceMismatch { .. }),
            "an offer open only to the terminal was answered from the web"
        );
    }

    #[test]
    fn a_workspace_mismatch_is_refused() {
        let def = definition();
        let mut inst = instance_for(&def);
        inst.scope.workspace_key = "ws-elsewhere".to_string();
        let live = published(&def, &inst);
        let response = response_for(&def, &inst, "deny", "k");
        let actions = registered();
        let refusal = validate_response(&def, &inst, &live, &response, &context(&actions))
            .expect_err("a cross-workspace response must be refused");
        assert!(
            matches!(refusal, Refusal::WorkspaceMismatch { .. }),
            "expected WorkspaceMismatch, got {refusal:?}"
        );
    }

    #[test]
    fn an_offer_requiring_an_assertion_refuses_an_unauthenticated_responder() {
        let def = definition();
        let mut inst = instance_for(&def);
        inst.responder_policy.requires_assertion = true;
        let live = published(&def, &inst);
        let mut response = response_for(&def, &inst, "deny", "k");
        response.responder_provenance.kind = AssertionKind::Unauthenticated;
        response.responder_provenance.assertion = None;
        let actions = registered();
        let refusal = validate_response(&def, &inst, &live, &response, &context(&actions))
            .expect_err("an unauthenticated responder must be refused");
        assert!(matches!(refusal, Refusal::AssertionRequired));
    }

    /// Atomicity: one bad control poisons the WHOLE response. No partial
    /// acceptance, and in particular no action resolved for the control
    /// that was fine.
    #[test]
    fn one_bad_control_rejects_the_whole_response() {
        let def = definition();
        let inst = instance_for(&def);
        let mut response = response_for(&def, &inst, "deny", "k");
        // A perfectly good optional control, plus one that is not
        // offered at all.
        response.values.push(Submission {
            control: ControlId::new("reason").unwrap(),
            value: ControlValue::Text {
                text: "because".to_string(),
            },
        });
        response.values.push(Submission {
            control: ControlId::new("smuggled").unwrap(),
            value: ControlValue::Toggle { on: true },
        });
        let refusal = refuse(&response);
        assert!(
            matches!(refusal, Refusal::ExtraControl { .. }),
            "expected ExtraControl, got {refusal:?}"
        );
    }
}

mod handlers {
    use super::*;

    #[test]
    fn an_action_maps_only_to_a_caller_registered_handler() {
        let def = definition();
        let inst = instance_for(&def);
        let live = published(&def, &inst);
        let response = response_for(&def, &inst, "deny", "k");

        // The caller's registry decides the handler name — twice, with
        // two different registries over the same definition and the same
        // response.
        for handler in ["gate::deny", "audit::record_denial"] {
            let actions = vec![RegisteredAction {
                option: OptionId::new("deny").unwrap(),
                handler: HandlerId::new(handler).unwrap(),
                audiences: vec![Audience::Terminal],
            }];
            let accepted = validate_response(&def, &inst, &live, &response, &context(&actions))
                .expect("valid");
            assert_eq!(accepted.actions.len(), 1);
            assert_eq!(accepted.actions[0].handler.as_str(), handler);
        }

        // With an empty registry there is no handler to route to, and
        // nothing runs.
        let refusal = validate_response(&def, &inst, &live, &response, &context(&[]))
            .expect_err("an empty registry routes nothing");
        assert!(matches!(refusal, Refusal::UnknownAction { .. }));
    }

    /// Nothing a document authored becomes executable. The definition
    /// below is stuffed with a command, a URL, a tool name, a path, a
    /// topic and a caveat; the accepted outcome must contain none of
    /// them — it names ids and the CALLER's handler, and nothing else.
    #[test]
    fn markup_supplied_command_url_tool_path_topic_or_caveats_are_never_executed() {
        const SMUGGLED: &[&str] = &[
            "rm -rf /",
            "https://evil.example/steal",
            "web_fetch",
            "/etc/passwd",
            "topic:exfiltrate",
            "caveats:none",
        ];

        let hostile = InteractionDefinition::new(
            InteractionKind::Choice,
            format!(
                "run {} and fetch {} via {} reading {} on {} with {}",
                SMUGGLED[0], SMUGGLED[1], SMUGGLED[2], SMUGGLED[3], SMUGGLED[4], SMUGGLED[5]
            ),
            vec![Control {
                id: ControlId::new("decision").unwrap(),
                kind: ControlKind::Choice {
                    options: vec![
                        option("allow-once", SemanticRole::Allow, SMUGGLED[0]),
                        option("deny", SemanticRole::Deny, SMUGGLED[1]),
                    ],
                },
                label: SMUGGLED[3].to_string(),
                requirement: newt_interaction::Requirement::Required,
            }],
        );
        let inst = instance_for(&hostile);
        let live = published(&hostile, &inst);
        let response = response_for(&hostile, &inst, "allow-once", "k");
        let actions = vec![RegisteredAction {
            option: OptionId::new("allow-once").unwrap(),
            handler: HandlerId::new("gate::allow_once").unwrap(),
            audiences: vec![Audience::Terminal],
        }];

        let accepted: Accepted =
            validate_response(&hostile, &inst, &live, &response, &context(&actions))
                .expect("the response itself is valid");
        let rendered = format!("{accepted:?}");
        for smuggled in SMUGGLED {
            assert!(
                !rendered.contains(smuggled),
                "the accepted outcome carried author-supplied text: {smuggled}"
            );
        }
        assert_eq!(accepted.actions[0].handler.as_str(), "gate::allow_once");
    }
}

mod resolution {
    use super::*;

    #[test]
    fn exactly_one_of_two_valid_responses_wins() {
        let store = InMemoryResolutions::default();
        let (def, inst) = (definition(), instance_for(&definition()));
        let first = record(&def, &inst, "deny", "key-1");
        let second = record(&def, &inst, "allow-once", "key-2");

        assert_eq!(store.resolve(&first).unwrap(), Resolution::Won);
        assert_eq!(
            store.resolve(&second).unwrap(),
            Resolution::Lost {
                winner: first.response
            }
        );
        assert_eq!(store.winner(&first.instance).unwrap(), Some(first.response));
    }

    #[test]
    fn the_loser_observes_the_same_terminal_state() {
        let store = InMemoryResolutions::default();
        let (def, inst) = (definition(), instance_for(&definition()));
        let winner = record(&def, &inst, "deny", "key-1");
        store.resolve(&winner).unwrap();

        // Every later racer is told the same thing: who won.
        for key in ["key-2", "key-3", "key-4"] {
            let loser = record(&def, &inst, "allow-once", key);
            assert_eq!(
                store.resolve(&loser).unwrap(),
                Resolution::Lost {
                    winner: winner.response
                }
            );
        }
        assert_eq!(
            store.winner(&winner.instance).unwrap(),
            Some(winner.response)
        );
    }

    #[test]
    fn a_replayed_response_does_not_resolve_twice() {
        let store = InMemoryResolutions::default();
        let (def, inst) = (definition(), instance_for(&definition()));
        let once = record(&def, &inst, "deny", "key-1");
        assert_eq!(store.resolve(&once).unwrap(), Resolution::Won);
        assert_eq!(
            store.resolve(&once).unwrap(),
            Resolution::Replayed {
                winner: once.response
            }
        );
        assert_eq!(store.resolutions(), 1, "a replay resolved a second time");
    }

    #[test]
    fn an_idempotency_key_collapses_a_retry() {
        let store = InMemoryResolutions::default();
        let (def, inst) = (definition(), instance_for(&definition()));
        let submission = record(&def, &inst, "deny", "retry-me");
        assert_eq!(store.resolve(&submission).unwrap(), Resolution::Won);
        // The identical submission, sent again after a timeout.
        for _ in 0..3 {
            assert_eq!(
                store.resolve(&submission).unwrap(),
                Resolution::Replayed {
                    winner: submission.response
                }
            );
        }
        assert_eq!(store.resolutions(), 1);
    }

    /// DECIDED: an idempotency key reused for a DIFFERENT submission is
    /// an ERROR, not first-wins. A key is a promise that the retry is
    /// the same submission; treating a different one as a retry would
    /// let a substituted answer ride in under a network retry's key.
    #[test]
    fn two_different_submissions_sharing_one_idempotency_key_conflict() {
        let store = InMemoryResolutions::default();
        let (def, inst) = (definition(), instance_for(&definition()));
        let first = record(&def, &inst, "deny", "same-key");
        let different = record(&def, &inst, "allow-once", "same-key");
        assert_ne!(first.response, different.response);

        assert_eq!(store.resolve(&first).unwrap(), Resolution::Won);
        let err = store
            .resolve(&different)
            .expect_err("a reused key with a different submission must not resolve");
        assert!(
            matches!(err, ResolutionError::IdempotencyConflict { .. }),
            "expected IdempotencyConflict, got {err:?}"
        );
        // The conflict changed nothing.
        assert_eq!(store.winner(&first.instance).unwrap(), Some(first.response));
        assert_eq!(store.resolutions(), 1);
    }

    fn record(
        def: &InteractionDefinition,
        inst: &InteractionInstance,
        option_id: &str,
        key: &str,
    ) -> ResolutionRecord {
        let response = response_for(def, inst, option_id, key);
        ResolutionRecord {
            instance: inst.instance_id().unwrap(),
            response: response.response_id().unwrap(),
            idempotency_key: IdempotencyKey::new(key).unwrap(),
        }
    }
}

/// An in-memory [`ResolutionStore`] — the contract, with no SQL shape.
///
/// Deliberately collapses to one compare-and-swap under a single lock:
/// the trait does not mandate the store's three-part transaction, and an
/// implementation that satisfies the observable contract satisfies it.
#[derive(Default)]
struct InMemoryResolutions {
    resolved: Mutex<BTreeMap<String, (ResponseId, String)>>,
}

impl InMemoryResolutions {
    fn resolutions(&self) -> usize {
        self.resolved.lock().unwrap().len()
    }
}

impl ResolutionStore for InMemoryResolutions {
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        record: &ResolutionRecord,
    ) -> Result<Resolution, ResolutionError<Self::Error>> {
        let mut resolved = self.resolved.lock().unwrap();
        let key = record.instance.to_string();
        match resolved.get(&key) {
            None => {
                resolved.insert(
                    key,
                    (record.response, record.idempotency_key.as_str().to_string()),
                );
                Ok(Resolution::Won)
            }
            Some((winner, used_key)) => {
                let same_key = used_key == record.idempotency_key.as_str();
                if winner == &record.response {
                    Ok(Resolution::Replayed { winner: *winner })
                } else if same_key {
                    Err(ResolutionError::IdempotencyConflict(Box::new(
                        newt_interaction::resolution::IdempotencyConflict {
                            key: used_key.clone(),
                            existing: *winner,
                            presented: record.response,
                        },
                    )))
                } else {
                    Ok(Resolution::Lost { winner: *winner })
                }
            }
        }
    }

    fn winner(&self, instance: &InstanceId) -> Result<Option<ResponseId>, Self::Error> {
        Ok(self
            .resolved
            .lock()
            .unwrap()
            .get(&instance.to_string())
            .map(|(winner, _)| *winner))
    }
}
