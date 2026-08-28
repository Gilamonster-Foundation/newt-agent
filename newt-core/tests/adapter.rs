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

mod b0a {
    use super::common;
    use std::path::Path;

    /// One required link in the call chain: inside `caller`'s body,
    /// `needle` must appear.
    struct Link {
        caller: &'static str,
        needle: &'static str,
        why: &'static str,
    }

    /// A path with its separators normalized to `/`.
    ///
    /// The shared scanner yields NATIVE paths, so on Windows this
    /// callback sees `newt-tui\\src\\permissions.rs`. Comparing that
    /// against a forward-slash suffix matches nothing, the scan returns
    /// zero lines, and the guard then has an empty file to reason about —
    /// which is why `the_definition_path_is_reached_from_both_surfaces`
    /// asserts the line list is non-empty BEFORE it asserts anything
    /// else. Without that check this bug would have been a silent green
    /// on Windows rather than a failure.
    ///
    /// Precedent: `markup_sprawl_ratchet::rel`, which normalizes the same
    /// way for the same reason — and is why the audience-confinement
    /// ratchet, which compares against `rel`-keyed map entries rather
    /// than raw paths, does not share this hazard.
    fn normalized(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    /// The production lines of one file, in order, as the shared scanner
    /// sees them (comments truncated, `#[cfg(test)]` regions skipped).
    fn production_lines(file_suffix: &str) -> Vec<String> {
        let roots = common::production_roots(&common::workspace_root());
        let mut lines = Vec::new();
        common::for_each_production_line(&roots, &|_| false, &mut |path, code, _raw| {
            if normalized(path).ends_with(file_suffix) {
                lines.push(code.to_string());
            }
        });
        lines
    }

    /// **Regression (#1841 review): a native Windows path must still match
    /// a forward-slash suffix.**
    ///
    /// CI failed on Windows — twice, not a flake — with "the scanner found
    /// no production lines in permissions.rs", because the suffix was
    /// compared against a backslash path. The failure was VISIBLE only
    /// because the guard asserts non-emptiness first: with no lines there
    /// are no missing links, so the guard would otherwise have reported
    /// green while proving nothing.
    #[test]
    fn a_windows_style_path_still_matches_its_forward_slash_suffix() {
        const SUFFIX: &str = "newt-tui/src/permissions.rs";
        assert!(
            normalized(Path::new(r"C:\repo\newt-tui\src\permissions.rs")).ends_with(SUFFIX),
            "a native Windows path did not match its forward-slash suffix"
        );
        assert!(normalized(Path::new("/home/x/newt-tui/src/permissions.rs")).ends_with(SUFFIX));
        // ...and it still discriminates, on both spellings.
        assert!(!normalized(Path::new(r"C:\repo\newt-tui\src\chat.rs")).ends_with(SUFFIX));
        assert!(!normalized(Path::new("/home/x/newt-tui/src/chat.rs")).ends_with(SUFFIX));
    }

    /// The body of `fn <name>`, delimited by BRACE DEPTH rather than by
    /// proximity.
    ///
    /// Depth is counted over `strip_string_literals`, which is load
    /// bearing here and not defensive: the policy's note strings contain
    /// `{MODAL_CONTROL_HINT}`, so a naive brace count closes the function
    /// early and the guard starts reading the next one's body.
    fn function_body(lines: &[String], name: &str) -> Option<String> {
        let anchor = format!("fn {name}(");
        let start = lines.iter().position(|line| line.contains(&anchor))?;
        let mut depth = 0i32;
        let mut opened = false;
        let mut body = String::new();
        for line in &lines[start..] {
            for ch in common::strip_string_literals(line).chars() {
                match ch {
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            body.push_str(line);
            body.push('\n');
            if opened && depth <= 0 {
                break;
            }
        }
        opened.then_some(body)
    }

    fn missing_links(lines: &[String], links: &[Link]) -> Vec<String> {
        let mut missing = Vec::new();
        for link in links {
            match function_body(lines, link.caller) {
                None => missing.push(format!("`fn {}` not found at all", link.caller)),
                Some(body) => {
                    if !body.contains(link.needle) {
                        missing.push(format!(
                            "`fn {}` does not reach `{}` — {}",
                            link.caller, link.needle, link.why
                        ));
                    }
                }
            }
        }
        missing
    }

    /// The chain each surface must walk to reach the one definition.
    const CHAIN: &[Link] = &[
        // B0b-1 (#1842) changed this link's SHAPE, not its property: the
        // terminal branch now builds the definition inline so the same
        // value it renders is the authority the answer is checked
        // against, rather than calling the `permission_question` facade
        // and losing the definition. The facade still exists and still
        // routes through `question_for`, which the next link pins.
        Link {
            caller: "ask",
            needle: "permission_definition(",
            why: "the terminal answer reader must be handed the built form",
        },
        Link {
            caller: "ask",
            needle: "definition_to_question(",
            why: "and must render it through the adapter, not a second renderer",
        },
        Link {
            caller: "permission_question",
            needle: "question_for(",
            why: "the TERMINAL surface must route through the definition path",
        },
        Link {
            caller: "await_web_decision",
            needle: "question_for(",
            why: "the WEB surface must route through the definition path",
        },
        Link {
            caller: "question_for",
            needle: "definition_to_question(",
            why: "rendering goes through the A2.2 adapter, not a second renderer",
        },
        Link {
            caller: "question_for",
            needle: "permission_definition(",
            why: "the adapter must be fed the ONE definition both surfaces build",
        },
        Link {
            caller: "permission_definition",
            needle: "InteractionDefinition::new(",
            why: "the definition is constructed here",
        },
    ];

    /// **B0a's positive guard, replacing A2.2's
    /// `no_production_path_uses_the_adapter_yet`.**
    ///
    /// A2.2 asserted the adapter had NO production callers. B0a switches
    /// both surfaces onto it, so the guard inverts — but **not as a
    /// one-line flip**. The shared scanner is per-line with no function
    /// or call-graph knowledge, so a bare "count >= 1" would pass green
    /// if only the web switched and the terminal quietly kept its own
    /// builder: exactly the regression this guard names. So each link is
    /// anchored to a NAMED function and checked inside that function's
    /// brace-delimited body, and the two surfaces are checked SEPARATELY.
    #[test]
    fn the_definition_path_is_reached_from_both_surfaces() {
        let lines = production_lines("newt-tui/src/permissions.rs");
        assert!(
            !lines.is_empty(),
            "the scanner found no production lines in permissions.rs"
        );
        let missing = missing_links(&lines, CHAIN);
        assert!(
            missing.is_empty(),
            "the definition path is not reached from both surfaces:\n{}",
            missing.join("\n")
        );

        // ...and the surfaces must each name their OWN audience, so a
        // switch that routed both through one hard-coded audience is a
        // failure rather than a pass.
        let terminal = function_body(&lines, "permission_question").expect("terminal entry point");
        assert!(
            terminal.contains("Audience::Terminal"),
            "the terminal entry point does not select the Terminal audience"
        );
        let web = function_body(&lines, "await_web_decision").expect("web construction site");
        assert!(
            web.contains("Audience::Web"),
            "the web construction site does not select the Web audience"
        );

        // The old builder is gone from the switched functions: a
        // surviving `Question {` literal there is the duplicate string
        // builder B0a deletes.
        for caller in [
            "permission_question",
            "question_for",
            "permission_definition",
        ] {
            let body = function_body(&lines, caller).expect("body");
            assert!(
                !body.contains("Question {"),
                "`fn {caller}` still constructs a Question directly"
            );
        }
    }

    /// **B0b-1 (#1842): the ACCEPT/DENY decision is reached from both
    /// surfaces, and lands on `validate_response`.**
    ///
    /// `spec/lint-behavior-map.py` already refuses a production ref whose
    /// named symbol is absent or ambiguous (`resolve_production` +
    /// `check_resolvable`), so the ORPHANED-ref half of the provenance
    /// obligation was already armed before this slice. What it cannot see
    /// is SEMANTIC drift: every symbol in BHV-PROMPT-001 still exists
    /// after the decision moves off it. This is the half that can fire —
    /// it pins where the decision actually goes, so moving it again
    /// without revisiting the behavior map breaks a test rather than
    /// silently leaving a `proven` claim over code that no longer decides.
    #[test]
    fn the_authorizer_is_reached_from_both_surfaces() {
        let gate = production_lines("newt-tui/src/permissions.rs");
        assert!(!gate.is_empty(), "no production lines in permissions.rs");
        let missing = missing_links(
            &gate,
            &[
                Link {
                    caller: "ask",
                    needle: "self.authorize(",
                    why: "the TERMINAL surface must authorize its decoded answer",
                },
                Link {
                    caller: "authorize",
                    needle: "authorize_action(",
                    why: "authorization is the controller's, not a downstream match arm's",
                },
            ],
        );
        assert!(missing.is_empty(), "{}", missing.join("\n"));

        let store = production_lines("newt-core/src/store.rs");
        assert!(!store.is_empty(), "no production lines in store.rs");
        let missing = missing_links(
            &store,
            &[Link {
                caller: "answer_permission_request_inner",
                needle: "web_answer_is_authorized(",
                why: "the WEB surface must authorize, not re-parse",
            }],
        );
        assert!(missing.is_empty(), "{}", missing.join("\n"));

        let gate_mod = production_lines("newt-core/src/interaction_gate.rs");
        assert!(
            !gate_mod.is_empty(),
            "no production lines in interaction_gate.rs"
        );
        let missing = missing_links(
            &gate_mod,
            &[
                Link {
                    caller: "web_answer_is_authorized",
                    needle: "authorize_action(",
                    why: "the web path shares the one authorizer",
                },
                Link {
                    caller: "authorize_action",
                    needle: "validate_response(",
                    why: "the decision lands on the formally-modelled validator",
                },
            ],
        );
        assert!(missing.is_empty(), "{}", missing.join("\n"));

        // The store must no longer DECIDE by parsing. `Question::parse` is
        // kept as an input decoder, but the answer transaction is not
        // where it decides any more.
        let answer = function_body(&store, "answer_permission_request_inner").expect("body");
        assert!(
            !answer.contains(".parse("),
            "the store's answer path still decides by parsing"
        );
    }

    /// **Anti-vacuous twin for the anchored guard.** The failure this
    /// guard exists to catch is a HALF switch, and a per-line "the
    /// adapter is called somewhere" check cannot see it. Feed the same
    /// machinery a source where only the web moved and the terminal kept
    /// its own builder: the guard must name the terminal, and must not
    /// be satisfied by the web's call.
    #[test]
    fn a_one_surface_switch_does_not_satisfy_the_guard() {
        let half_switched: Vec<String> = [
            "fn permission_question(req: &R, danger: &D) -> Question<PromptChoice> {",
            "    Question {",
            "        markdown: format!(\"{} wants\", req.tool),",
            "        actions: vec![],",
            "        note: None,",
            "    }",
            "}",
            "fn await_web_decision(&self, req: &R) -> (PromptChoice, &'static str) {",
            "    let question = question_for(req, &self.danger, Audience::Web);",
            "}",
            "fn question_for(req: &R, danger: &D, audience: Audience) -> Question<PromptChoice> {",
            "    definition_to_question(&permission_definition(req, danger, audience)).expect(\"x\")",
            "}",
            "fn permission_definition(req: &R) -> InteractionDefinition {",
            "    InteractionDefinition::new(kind, markdown, controls)",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let missing = missing_links(&half_switched, CHAIN);
        assert!(
            missing
                .iter()
                .any(|m| m.contains("permission_question") && m.contains("question_for(")),
            "the guard did not notice that the TERMINAL surface never switched: {missing:#?}"
        );
        // The web half really is switched — so the guard is discriminating
        // between the surfaces, not just failing on everything.
        assert!(
            !missing.iter().any(|m| m.contains("await_web_decision")),
            "the guard flagged the web surface, which IS switched: {missing:#?}"
        );
        // And the old builder survives in the unswitched function, which
        // the brace-depth body extraction must attribute to the right
        // function rather than to its neighbour.
        let terminal = function_body(&half_switched, "permission_question").expect("body");
        assert!(terminal.contains("Question {"));
        let web = function_body(&half_switched, "await_web_decision").expect("body");
        assert!(
            !web.contains("Question {"),
            "the body extractor bled the previous function into this one"
        );
    }
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
