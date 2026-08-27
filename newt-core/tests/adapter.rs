//! **The legacy `Question<A>` maps onto an `InteractionDefinition` without
//! losing a byte** (A2.2, #1828).
//!
//! A2's whole claim is that one semantic model can feed every view. The
//! adapter is where that claim meets the model already in production, and
//! the test of it is not "does it compile" but "does a `Question` survive
//! the trip and serialize to the SAME BYTES A0 froze". Anything less and
//! the migration would silently rewrite the wire the web store and the
//! terminal already share.
//!
//! Production still runs the old path. A2.2 proves losslessness; switching
//! is B0's job, and `no_production_path_uses_the_adapter_yet` holds that
//! line mechanically.

use newt_core::interaction_adapter::{definition_to_question, question_to_definition};
use newt_core::{Action, PermissionAction, Question};

mod common;

/// Every `PermissionAction`, so the property is over the whole set rather
/// than one hand-picked case.
const ALL_ACTIONS: &[(PermissionAction, &str, &str)] = &[
    (PermissionAction::AllowOnce, "a", "allow once"),
    (PermissionAction::AllowSession, "s", "session allow"),
    (PermissionAction::AllowPermanent, "A", "Allow permanently"),
    (PermissionAction::Deny, "d", "deny (default)"),
    (PermissionAction::DenyAlways, "D", "Deny always"),
    (PermissionAction::DenyPermanent, "P", "Permanently deny"),
    (PermissionAction::Back, "b", "back"),
    (PermissionAction::Exit, "x", "exit"),
];

/// A0's frozen goldens, verbatim. If a byte here would change, the
/// adapter is wrong — these are the shared wire, not this slice's.
const FULL_GOLDEN: &str = r#"{"markdown":"⊘ run_command wants to run `bash`","actions":[{"value":"allow_once","key":"a","label":"allow once"},{"value":"deny","key":"d","label":"deny (default)","aliases":["n","N"]}],"note":"Esc=back"}"#;
const MINIMAL_GOLDEN: &str =
    r#"{"markdown":"m","actions":[{"value":"deny","key":"d","label":"deny"}]}"#;

fn full_question() -> Question<PermissionAction> {
    Question {
        markdown: "\u{2298} run_command wants to run `bash`".to_string(),
        actions: vec![
            Action::new(PermissionAction::AllowOnce, "a", "allow once"),
            Action::new(PermissionAction::Deny, "d", "deny (default)").with_aliases(["n", "N"]),
        ],
        note: Some("Esc=back".to_string()),
    }
}

fn minimal_question() -> Question<PermissionAction> {
    Question {
        markdown: "m".to_string(),
        actions: vec![Action::new(PermissionAction::Deny, "d", "deny")],
        note: None,
    }
}

#[test]
fn a_question_round_trips_through_the_definition_byte_for_byte() {
    // The populated form: markdown with a ⊘ glyph and backticks, an action
    // carrying hidden aliases, and a note.
    let definition = question_to_definition(&full_question()).expect("adapts");
    let back = definition_to_question(&definition).expect("adapts back");
    assert_eq!(back, full_question(), "the value changed in transit");
    assert_eq!(
        serde_json::to_string(&back).unwrap(),
        FULL_GOLDEN,
        "the round trip changed A0's frozen wire bytes"
    );

    // The minimal form, which is what pins both `skip_serializing_if`s:
    // empty `aliases` and a `None` note must still be OMITTED after the
    // trip, not rendered as `[]` and `null`.
    let definition = question_to_definition(&minimal_question()).expect("adapts");
    let back = definition_to_question(&definition).expect("adapts back");
    assert_eq!(
        serde_json::to_string(&back).unwrap(),
        MINIMAL_GOLDEN,
        "the round trip resurrected an omitted field"
    );

    // The pre-aliases payload A0 froze: it must deserialize, survive the
    // trip, and still authorize exactly its displayed action.
    let legacy: Question<PermissionAction> =
        serde_json::from_str(MINIMAL_GOLDEN).expect("pre-aliases payloads deserialize");
    let round_tripped =
        definition_to_question(&question_to_definition(&legacy).expect("adapts")).expect("back");
    assert_eq!(round_tripped.parse("d"), Some(PermissionAction::Deny));
    assert_eq!(
        round_tripped.parse("a"),
        None,
        "an undisplayed action must not become parseable by adapting"
    );
}

/// The whole action set, over both frozen surface matrices.
#[test]
fn every_action_survives_the_round_trip() {
    // A0 froze the terminal and web matrices as deliberately DIFFERENT, so
    // both shapes are exercised: the full terminal set, and the web subset
    // that omits every durable grant.
    let terminal: Vec<Action<PermissionAction>> = ALL_ACTIONS
        .iter()
        .map(|(value, key, label)| Action::new(*value, *key, *label))
        .collect();
    let web: Vec<Action<PermissionAction>> = ALL_ACTIONS
        .iter()
        .filter(|(value, _, _)| {
            matches!(
                value,
                PermissionAction::AllowOnce
                    | PermissionAction::AllowSession
                    | PermissionAction::Deny
            )
        })
        .map(|(value, key, label)| Action::new(*value, *key, *label))
        .collect();

    for (surface, actions) in [("terminal", terminal), ("web", web)] {
        let question = Question {
            markdown: format!("a {surface} prompt"),
            actions,
            note: Some("Esc=back · Ctrl-C/Ctrl-D=exit".to_string()),
        };
        let back = definition_to_question(&question_to_definition(&question).expect("adapts"))
            .expect("back");
        assert_eq!(back, question, "{surface} matrix did not survive");
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            serde_json::to_string(&question).unwrap(),
            "{surface} matrix serialized differently after the trip"
        );
    }
}

#[test]
fn the_adapter_preserves_parse_semantics() {
    // Every action by key, by wire value, and by alias — plus the
    // ambiguity denial, which is backed by the Lean `authorization_sound`
    // and TLA+ `AuthorizationDisplayed` models and must not weaken.
    let question = Question {
        markdown: "every action".to_string(),
        actions: ALL_ACTIONS
            .iter()
            .map(|(value, key, label)| {
                Action::new(*value, *key, *label).with_aliases([format!("alias-{key}")])
            })
            .collect(),
        note: None,
    };
    let back =
        definition_to_question(&question_to_definition(&question).expect("adapts")).expect("back");

    for (value, key, _) in ALL_ACTIONS {
        for input in [
            (*key).to_string(),
            value.as_str().to_string(),
            format!("alias-{key}"),
        ] {
            assert_eq!(
                back.parse(&input),
                question.parse(&input),
                "parse disagreed on {input:?} after adapting"
            );
            assert_eq!(back.parse(&input), Some(*value), "parse lost {input:?}");
        }
    }
    // Undisplayed input denies, before and after.
    for input in ["", " ", "nope", "alias-zzz"] {
        assert_eq!(back.parse(input), None);
        assert_eq!(back.parse(input), question.parse(input));
    }

    // Ambiguity: two actions sharing a key deny rather than selecting the
    // earlier one, and the adapter must not "helpfully" de-duplicate.
    let ambiguous = Question {
        markdown: "ambiguous".to_string(),
        actions: vec![
            Action::new(PermissionAction::AllowOnce, "a", "allow"),
            Action::new(PermissionAction::Deny, "a", "deny"),
        ],
        note: None,
    };
    assert_eq!(ambiguous.parse("a"), None, "the fixture is not ambiguous");
    let adapted =
        definition_to_question(&question_to_definition(&ambiguous).expect("adapts")).expect("back");
    assert_eq!(
        adapted.parse("a"),
        None,
        "adapting resolved an ambiguity that must deny"
    );

    // An alias must never shadow another action's canonical key.
    let shadowing = Question {
        markdown: "shadow".to_string(),
        actions: vec![
            Action::new(PermissionAction::AllowOnce, "a", "allow"),
            Action::new(PermissionAction::Deny, "d", "deny").with_aliases(["a"]),
        ],
        note: None,
    };
    let adapted =
        definition_to_question(&question_to_definition(&shadowing).expect("adapts")).expect("back");
    assert_eq!(adapted.parse("a"), Some(PermissionAction::AllowOnce));
    assert_eq!(adapted.parse("a"), shadowing.parse("a"));
}

/// **A2 must not switch production.** The adapter exists to prove
/// losslessness while everything still runs the old path; flipping the
/// switch is B0's slice, with its own deletion gate.
#[test]
fn no_production_path_uses_the_adapter_yet() {
    let roots = common::production_roots(&common::workspace_root());
    let mut callers = Vec::new();
    common::for_each_production_line(&roots, &|_| false, &mut |path, code, raw| {
        // The module's own definition is not a caller of itself.
        if path.ends_with("interaction_adapter.rs") {
            return;
        }
        for needle in ["question_to_definition(", "definition_to_question("] {
            if code.contains(needle) {
                callers.push(format!("{}: {}", path.display(), raw.trim()));
            }
        }
    });
    assert!(
        callers.is_empty(),
        "the adapter has production callers, so A2.2 has switched production \
         ahead of B0: {callers:#?}"
    );
}

/// **Anti-vacuous twin.** A scanner that sees nothing reports "no callers"
/// forever, which is indistinguishable from the state this test wants.
#[test]
fn the_caller_scan_sees_a_seeded_call() {
    let root = tempfile::tempdir().unwrap();
    let src = root.path().join("newt-cli/src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"newt-cli\"]\n",
    )
    .unwrap();
    std::fs::write(
        src.join("main.rs"),
        "fn f() { let _ = question_to_definition(&q); }\n",
    )
    .unwrap();

    let mut seen = 0usize;
    common::for_each_production_line(
        &common::production_roots(root.path()),
        &|_| false,
        &mut |_, code, _| {
            if code.contains("question_to_definition(") {
                seen += 1;
            }
        },
    );
    assert_eq!(seen, 1, "the scanner missed a seeded adapter call");
}
