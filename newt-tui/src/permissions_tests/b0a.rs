use super::*;
use newt_core::{DenialKind, PermissionRequest};

fn net_low() -> PermissionRequest {
    PermissionRequest {
        tool: "http".into(),
        kind: DenialKind::Net,
        target: "https://example.com/api".into(),
        reason: String::new(),
    }
}

fn exec_high() -> PermissionRequest {
    PermissionRequest {
        tool: "run_command".into(),
        kind: DenialKind::Exec,
        target: "bash".into(),
        reason: "exec of \"bash\" is not within the granted authority".into(),
    }
}

/// The option wire names a definition offers, in order.
fn offered(definition: &InteractionDefinition) -> Vec<String> {
    let [control] = definition.controls.as_slice() else {
        panic!("expected exactly one control");
    };
    let ControlKind::Choice { options } = &control.kind else {
        panic!("the decision control is not a choice");
    };
    options.iter().map(|o| o.id.as_str().to_string()).collect()
}

fn definition_of(req: &PermissionRequest, audience: Audience) -> InteractionDefinition {
    permission_definition(req, &danger::DangerTable::builtin(), audience)
}

#[test]
fn both_surfaces_build_one_definition_from_one_builder() {
    for req in [net_low(), exec_high()] {
        for audience in [Audience::Terminal, Audience::Web] {
            let definition = definition_of(&req, audience.clone());
            // ONE definition, ONE control, and it is the reserved
            // decision control the adapter and #1842 both address.
            assert_eq!(definition.controls.len(), 1);
            assert_eq!(definition.controls[0].id.as_str(), DECISION_CONTROL);
            assert!(matches!(
                definition.controls[0].kind,
                ControlKind::Choice { .. }
            ));
            // The kind now AGREES WITH THE SHAPE rather than being a
            // constant (#1912): this fixture offers two actions, one
            // granting and one refusing, which is a binary decision.
            // Asserting the derivation rather than the literal is what
            // keeps this honest if the fixture gains a third action.
            assert_eq!(
                definition.kind,
                if definition.is_decision_shaped() {
                    InteractionKind::Confirm
                } else {
                    InteractionKind::Choice
                },
                "the permission builder's kind must follow its controls"
            );
            assert_eq!(definition.controls[0].requirement, Requirement::Required);

            // D0 (#1878): this compared the definition against the
            // adapter's reconstruction of it. The reconstruction is
            // deleted, so the property is stated directly: the ONE
            // builder is what both surfaces call, and calling it twice
            // for the same inputs is the same definition.
            assert_eq!(
                permission_definition(&req, &danger::DangerTable::builtin(), audience.clone()),
                definition,
                "the builder is not a pure function of its inputs"
            );
        }
    }
    // The two surfaces differ only where policy says they do.
    assert_ne!(
        offered(&definition_of(&net_low(), Audience::Terminal)),
        offered(&definition_of(&net_low(), Audience::Web)),
        "the surfaces would be indistinguishable, so the matrices are not being applied"
    );
}

#[test]
fn the_terminal_matrix_is_byte_identical_to_its_a0_golden() {
    let definition = definition_of(&net_low(), Audience::Terminal);
    assert_eq!(
        offered(&definition),
        [
            "allow_once",
            "allow_session",
            "allow_permanent",
            "deny",
            "deny_always",
            "deny_permanent"
        ]
    );
    assert_eq!(
            plain::render(&definition),
            "\u{2298} http wants to reach `https://example.com/api` \u{2014} outside the granted net allowlist.\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once\n\
             [s]ession allow\n\
             [A]llow permanently (adds host to config)\n\
             [d]eny (default)\n\
             [D]eny always\n\
             [P]ermanently deny"
        );
}

#[test]
fn the_web_matrix_is_byte_identical_to_its_a0_golden() {
    let definition = definition_of(&net_low(), Audience::Web);
    assert_eq!(
        offered(&definition),
        ["allow_once", "allow_session", "deny"]
    );
    assert_eq!(
            plain::render(&definition),
            "\u{2298} http wants to reach `https://example.com/api` \u{2014} outside the granted net allowlist.\n\
             Esc=back \u{b7} Ctrl-C/Ctrl-D=exit\n\
             [a]llow once\n\
             [s]ession allow\n\
             [d]eny (default)"
        );
}

#[test]
fn a_high_tier_target_offers_no_session_allow_on_either_surface() {
    for audience in [Audience::Terminal, Audience::Web] {
        let offered = offered(&definition_of(&exec_high(), audience.clone()));
        assert!(
            !offered.iter().any(|id| id == "allow_session"),
            "a high-danger target offered a session allow to {audience:?}: {offered:?}"
        );
        assert!(
            !offered.iter().any(|id| id == "allow_permanent"),
            "a high-danger target offered a permanent allow to {audience:?}: {offered:?}"
        );
        // ...and it still offers a way to say yes once, or the prompt
        // would be a notice rather than a decision.
        assert!(offered.iter().any(|id| id == "allow_once"));
    }
}

#[test]
fn the_web_definition_never_offers_a_durable_grant() {
    const DURABLE: [&str; 3] = ["allow_permanent", "deny_always", "deny_permanent"];
    for req in [net_low(), exec_high()] {
        let offered = offered(&definition_of(&req, Audience::Web));
        for durable in DURABLE {
            assert!(
                !offered.iter().any(|id| id == durable),
                "the web definition offered `{durable}` for {}: {offered:?}",
                req.target
            );
        }
    }
    // The terminal still does, for the one case A0 froze — otherwise
    // this test would pass by the grants having been deleted for
    // everyone.
    assert!(offered(&definition_of(&net_low(), Audience::Terminal))
        .iter()
        .any(|id| id == "allow_permanent"));
}

/// Aliases and ambiguity denial are properties of `Question::parse`,
/// which B0a leaves authoritative. What changes is that the form now
/// arrives through a definition — so the property must survive the
/// round trip.
#[test]
fn an_alias_still_resolves_and_an_ambiguous_answer_is_still_denied() {
    // D0 (#1878): asserted through `newt_interaction::binding::resolve_typed`,
    // which now owns the canonical-first / alias / ambiguity-denial rules.
    // The general cases live with it (`resolve_typed_tests`); this is the
    // PERMISSION-surface instance, kept here because it is this surface's
    // fail-closed behaviour that matters.
    let opt = |wire: &str, role, key: &str, alias: &str| newt_interaction::ChoiceOption {
        id: newt_interaction::OptionId::new(wire).expect("valid"),
        role,
        label: wire.to_string(),
        key: key.to_string(),
        aliases: vec![alias.to_string()],
    };
    let with_alias = vec![
        opt(
            "allow_once",
            newt_interaction::SemanticRole::Allow,
            "y",
            "Y",
        ),
        opt("deny", newt_interaction::SemanticRole::Deny, "n", "N"),
    ];
    assert_eq!(
        newt_interaction::binding::resolve_typed(&with_alias, "Y")
            .and_then(|o| action_for_option(o.as_str())),
        Some(PromptChoice::AllowOnce)
    );
    assert_eq!(
        newt_interaction::binding::resolve_typed(&with_alias, "n")
            .and_then(|o| action_for_option(o.as_str())),
        Some(PromptChoice::Deny)
    );

    // Ambiguity still denies: two options sharing one alias resolve to
    // nothing, and the caller's fail-closed default stands.
    let ambiguous = vec![
        opt(
            "allow_once",
            newt_interaction::SemanticRole::Allow,
            "y",
            "x",
        ),
        opt("deny", newt_interaction::SemanticRole::Deny, "n", "x"),
    ];
    assert_eq!(
        newt_interaction::binding::resolve_typed(&ambiguous, "x"),
        None,
        "an ambiguous answer resolved"
    );

    // And the real permission menu still parses its own keys.
    let menu = permission_definition(
        &net_low(),
        &danger::DangerTable::builtin(),
        Audience::Terminal,
    );
    let resolve = |input: &str| {
        let ControlKind::Choice { options } = &menu.controls[0].kind else {
            panic!("not a choice");
        };
        newt_interaction::binding::resolve_typed(options, input)
            .and_then(|o| action_for_option(o.as_str()))
    };
    assert_eq!(resolve("a"), Some(PromptChoice::AllowOnce));
    assert_eq!(resolve("A"), Some(PromptChoice::AllowPermanent));
    assert_eq!(resolve("zzz"), None);
}

/// The `expect`s in `permission_definition` are
/// unreachable rather than merely unlikely: every combination this
/// policy can produce builds and adapts.
#[test]
fn every_offered_action_is_a_valid_option_id() {
    let kinds = [
        (DenialKind::Exec, "npm"),
        (DenialKind::Exec, "bash"),
        (DenialKind::FsRead, "/etc/passwd"),
        (DenialKind::FsWrite, "/tmp/x"),
        (DenialKind::Net, "https://example.com/api"),
        (DenialKind::RemoteTool, "some_tool"),
        (DenialKind::GitWrite, "origin/main"),
    ];
    for (kind, target) in kinds {
        for audience in [Audience::Terminal, Audience::Web] {
            let req = PermissionRequest {
                tool: "t".into(),
                kind,
                target: target.into(),
                reason: String::new(),
            };
            let definition = definition_of(&req, audience.clone());
            assert!(!offered(&definition).is_empty());
            // D0 (#1878): "does it adapt" became "does every offered
            // option name an action this build knows" — the property the
            // adapter's error arm actually stood for, asserted without it.
            let ControlKind::Choice { options } = &definition.controls[0].kind else {
                panic!("{kind:?}/{target}/{audience:?} is not a choice");
            };
            for option in options {
                assert!(
                    action_for_option(option.id.as_str()).is_some(),
                    "{kind:?}/{target}/{audience:?} offers unknown option {}",
                    option.id.as_str()
                );
            }
        }
    }
}
