//! **A3's persistence half** (#1837): the SQLite-backed
//! [`ResolutionStore`] implementation.
//!
//! The pure rules are tested where they live, in
//! `newt-interaction/tests/controller.rs`. What can only be tested HERE is
//! the thing SQLite provides and an in-memory map cannot: exactly-once
//! across INDEPENDENT CONNECTIONS.
//!
//! **Why not two threads sharing one store.** `ConversationStore` holds
//! `conn: Arc<Mutex<Connection>>`, so every method on one instance is
//! already serialized in-process. Two threads sharing a store would
//! exercise that Mutex and prove nothing about the database — a vacuous
//! green. The race that actually exists is cross-connection: `newt-web`
//! opens a fresh `ConversationStore::new` on every HTTP request, so the
//! TTY-owning process and each web request hold independent connections.
//! These tests follow the repo's own precedent for that shape,
//! `store.rs::concurrent_store_connections_serialize_prompt_predecessors`
//! — a store per thread, plus a `Barrier`.

use std::sync::{Arc, Barrier};

use newt_core::store::ConversationStore;
use newt_interaction::resolution::{
    Resolution, ResolutionError, ResolutionRecord, ResolutionStore,
};
use newt_interaction::{
    AssertionKind, Audience, ChoiceOption, Control, ControlId, ControlKind, ControlValue,
    IdempotencyKey, InteractionDefinition, InteractionInstance, InteractionKind, Nonce, OptionId,
    Provenance, Requirement, ResponderPolicy, ResponderProvenance, Response, Scope, SemanticRole,
    Submission,
};

fn definition() -> InteractionDefinition {
    InteractionDefinition::new(
        // Confirm, not Choice (#1912): one control, two options, `Allow` and
        // `Deny`. A permission decision — a grant and a refusal — not a pick
        // from a displayed set. A3's resolution fixtures carried the same
        // defect C0c found in `agentic/tools.rs`, which makes it a THIRD
        // hand-written site and not a coincidence.
        InteractionKind::Confirm,
        "⊘ run_command wants to run `bash`",
        vec![Control {
            id: ControlId::new("decision").unwrap(),
            kind: ControlKind::Choice {
                options: vec![
                    ChoiceOption {
                        id: OptionId::new("allow-once").unwrap(),
                        role: SemanticRole::Allow,
                        label: "allow once".to_string(),
                        key: String::new(),
                        aliases: Vec::new(),
                    },
                    ChoiceOption {
                        id: OptionId::new("deny").unwrap(),
                        role: SemanticRole::Deny,
                        label: "deny".to_string(),
                        key: String::new(),
                        aliases: Vec::new(),
                    },
                ],
            },
            label: "what should happen".to_string(),
            requirement: Requirement::Required,
        }],
    )
}

fn instance(def: &InteractionDefinition) -> InteractionInstance {
    InteractionInstance {
        schema: newt_interaction::InstanceTag,
        nonce: Nonce::new("1756200000000000000-0f4c1b2e").unwrap(),
        definition: def.definition_id().unwrap(),
        revision: def.revision,
        ttl_ticks: 300,
        scope: Scope {
            workspace_key: "ws".to_string(),
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

fn record(
    def: &InteractionDefinition,
    inst: &InteractionInstance,
    option: &str,
    key: &str,
) -> ResolutionRecord {
    let response = Response {
        schema: newt_interaction::ResponseTag,
        definition: def.definition_id().unwrap(),
        instance: inst.instance_id().unwrap(),
        revision: def.revision,
        values: vec![Submission {
            control: ControlId::new("decision").unwrap(),
            value: ControlValue::Choice {
                option: OptionId::new(option).unwrap(),
            },
        }],
        idempotency_key: IdempotencyKey::new(key).unwrap(),
        responder_provenance: ResponderProvenance {
            kind: AssertionKind::TerminalOperator,
            subject: "operator".to_string(),
            audience: Audience::Terminal,
            assertion: Some("tty-1".to_string()),
        },
    };
    ResolutionRecord {
        instance: inst.instance_id().unwrap(),
        response: response.response_id().unwrap(),
        idempotency_key: IdempotencyKey::new(key).unwrap(),
    }
}

mod resolution {
    use super::*;

    /// **The real race.** Two INDEPENDENT connections, one per thread —
    /// the shape `newt-web` actually produces — released together by a
    /// `Barrier`. Exactly one must win, and the loser must be told who
    /// did.
    #[test]
    fn separate_connections_racing_one_instance_resolve_once() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let def = definition();
        let inst = instance(&def);

        // Two genuinely different submissions, with different keys, so a
        // loser is a Lost rather than an idempotency conflict.
        let contenders = [
            record(&def, &inst, "deny", "key-left"),
            record(&def, &inst, "allow-once", "key-right"),
        ];

        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for contender in contenders.clone() {
            let root_path = root.path().to_path_buf();
            let workspace_path = workspace.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                // A FRESH store — its own connection, exactly as a web
                // request gets.
                let store = ConversationStore::new(root_path, workspace_path, 100).unwrap();
                barrier.wait();
                store.resolve(&contender)
            }));
        }
        barrier.wait();
        let outcomes: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();

        let won: Vec<_> = outcomes
            .iter()
            .filter(|o| matches!(o, Ok(Resolution::Won)))
            .collect();
        assert_eq!(
            won.len(),
            1,
            "exactly one connection must win; got {outcomes:#?}"
        );

        let reopened = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let winner = reopened
            .winner(&inst.instance_id().unwrap())
            .unwrap()
            .expect("the offer is resolved");

        // The loser was told who won, and it is the same fact the store
        // reports afterwards.
        let losers: Vec<_> = outcomes
            .iter()
            .filter_map(|o| match o {
                Ok(Resolution::Lost { winner }) => Some(*winner),
                _ => None,
            })
            .collect();
        assert_eq!(losers.len(), 1, "the other connection must Lose");
        assert_eq!(losers[0], winner, "the loser observed a different winner");
        assert!(
            contenders.iter().any(|c| c.response == winner),
            "the winner is not one of the racers"
        );
    }

    /// The same three outcomes, through the database rather than the
    /// in-memory contract implementation.
    #[test]
    fn the_store_wins_loses_and_replays_like_the_contract_says() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let def = definition();
        let inst = instance(&def);

        let first = record(&def, &inst, "deny", "key-1");
        let second = record(&def, &inst, "allow-once", "key-2");

        assert_eq!(store.resolve(&first).unwrap(), Resolution::Won);
        assert_eq!(
            store.resolve(&second).unwrap(),
            Resolution::Lost {
                winner: first.response
            }
        );
        assert_eq!(
            store.resolve(&first).unwrap(),
            Resolution::Replayed {
                winner: first.response
            }
        );
        assert_eq!(
            store.winner(&inst.instance_id().unwrap()).unwrap(),
            Some(first.response)
        );
    }

    /// The decided semantics, in the database: one key, two different
    /// submissions is an ERROR, not a quiet first-wins.
    #[test]
    fn the_store_refuses_a_reused_idempotency_key() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let def = definition();
        let inst = instance(&def);

        let first = record(&def, &inst, "deny", "same-key");
        let different = record(&def, &inst, "allow-once", "same-key");
        assert_ne!(first.response, different.response);

        assert_eq!(store.resolve(&first).unwrap(), Resolution::Won);
        let err = store
            .resolve(&different)
            .expect_err("a reused key with a different submission must not resolve");
        assert!(
            matches!(err, ResolutionError::IdempotencyConflict(_)),
            "expected IdempotencyConflict, got {err:?}"
        );
        assert_eq!(
            store.winner(&inst.instance_id().unwrap()).unwrap(),
            Some(first.response),
            "the conflict changed the winner"
        );
    }

    /// A store that has resolved nothing reports no winner — so a green
    /// `winner()` assertion elsewhere is not green by default.
    #[test]
    fn an_unresolved_offer_has_no_winner() {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = ConversationStore::new(root.path(), workspace.path(), 100).unwrap();
        let def = definition();
        let inst = instance(&def);
        assert_eq!(store.winner(&inst.instance_id().unwrap()).unwrap(), None);
    }
}
