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

mod common;

// **D0 (#1878): the round-trip tests are GONE, because the round trip is.**
//
// `definition_to_question` — the reverse direction — is deleted: its last two
// production callers were `decode_answer`, which now resolves through
// `newt_interaction::binding::resolve_typed`, and a renderability precondition
// in `await_web_decision` whose own comment said it "retires with C3's removal
// of the reconstruction", which C3c did. A test that a deleted function
// round-trips is not a test that needs retargeting; the property it asserted
// no longer exists.
//
// What the property GOVERNED did not vanish with it. The canonical-first /
// alias / ambiguity-denial rules moved to `resolve_typed`, and their coverage
// moved with them — see `newt_interaction::binding::resolve_typed_tests`,
// which carries the same cases under the same names plus the two the old
// suite proved through the adapter (`an_alias_never_shadows_another_options_
// wire_name`, `key_value_collision_between_options_fails_closed`).

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
    pub(super) fn function_body(lines: &[String], name: &str) -> Option<String> {
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
        // C0a (#1856) MOVED this link rather than deleting it. `ask` used
        // to adapt the definition to a `Question` and hand THAT to the
        // reader; it now hands the definition itself, and the rendering
        // happens at the read site. The property — "the terminal renders
        // the one definition, not a second builder" — is unchanged, so the
        // link follows the rendering to where it went. Deleting it instead
        // would have left the terminal's rendering pinned by nothing.
        Link {
            caller: "prompt_permission_choice",
            needle: "plain::render(",
            why: "the terminal read site must render through the ONE plain projection",
        },
        Link {
            caller: "prompt_permission_choice",
            needle: "decode_answer(",
            why: "and decode through the one parser, not a second validator",
        },
        // B0b-2 (#1846): the web surface now PUBLISHES the definition, so
        // it builds one directly instead of going through the rendering
        // facade. Both surfaces still reach the same builder; only the web
        // additionally persists it.
        Link {
            caller: "await_web_decision",
            needle: "permission_definition(",
            why: "the WEB surface must route through the definition path",
        },
        // D0 (#1878): the Link requiring `definition_to_question(` here is
        // REMOVED. It guarded a renderability precondition whose own comment
        // said it "retires with C3's removal of the reconstruction" — C3c
        // removed that reconstruction, so the precondition was guarding a
        // rendering path that no longer exists, and the function it called is
        // deleted. The web surface still routes through the definition path;
        // the two Links either side of this comment are what hold that.
        Link {
            caller: "await_web_decision",
            needle: "publish_interaction_offer(",
            why: "the web surface publishes the OFFER, not a bare question",
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
        let terminal = function_body(&lines, "ask").expect("terminal entry point");
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
        for caller in ["permission_definition", "await_web_decision"] {
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

        let store = production_lines("newt-core/src/interaction_offer.rs");
        assert!(
            !store.is_empty(),
            "no production lines in interaction_offer.rs"
        );
        let missing = missing_links(
            &store,
            &[Link {
                caller: "answer_interaction_offer",
                needle: "authorized_response(",
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
            &[Link {
                caller: "authorize_action",
                needle: "validate_response(",
                why: "the decision lands on the formally-modelled validator",
            }],
        );
        assert!(missing.is_empty(), "{}", missing.join("\n"));

        // The store must no longer DECIDE by parsing. `Question::parse` is
        // kept as an input decoder, but the answer transaction is not
        // where it decides any more.
        let answer = function_body(&store, "answer_interaction_offer").expect("body");
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
            // The TERMINAL surface never switched: `ask` still builds its
            // own Question instead of the one definition.
            "fn ask(&mut self, requests: &[R]) -> Decision {",
            "    let q = Question {",
            "        markdown: format!(\"{} wants\", req.tool),",
            "        actions: vec![],",
            "        note: None,",
            "    };",
            "    let choice = (self.ask_human)(&w, &q);",
            "}",
            "fn await_web_decision(&self, req: &R) -> (PromptChoice, &'static str) {",
            "    let definition = permission_definition(req, &self.danger, Audience::Web);",
            "    let id = store.publish_interaction_offer(&conv, &definition, tier, Audience::Web);",
            "}",
            // C0a: the read site, switched. Present so the twin's
            // "the web half is NOT flagged" assertion cannot pass merely
            // because every C0a link is missing from the fixture.
            "fn prompt_permission_choice(w: &PromptWindow, d: &InteractionDefinition) -> PromptChoice {",
            "    let prompt = format!(\"{}\", plain::render(d));",
            "    decode_answer(d, &answer)",
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
                .any(|m| m.contains("ask") && m.contains("permission_definition(")),
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
        let terminal = function_body(&half_switched, "ask").expect("body");
        assert!(terminal.contains("Question {"));
        let web = function_body(&half_switched, "await_web_decision").expect("body");
        assert!(
            !web.contains("Question {"),
            "the body extractor bled the previous function into this one"
        );
    }
}

/// **C0a (#1856): rendering left the semantic type.**
///
/// The epic's named C0 deletion gate is *"semantic core types no longer
/// expose terminal rendering"*. Two facts make that checkable, and each
/// carries an anti-vacuous twin — a source scan that sees nothing reports
/// "deleted" for the same reason it reports "clean", which is precisely the
/// vacuous shape B0a's Windows path-separator bug produced (see
/// `b0a::normalized`).
mod c0a {
    use super::common;
    use std::path::Path;

    fn normalized(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    /// Every production line of the workspace, as `(normalized path, code)`.
    fn production_source() -> Vec<(String, String)> {
        let roots = common::production_roots(&common::workspace_root());
        let mut lines = Vec::new();
        common::for_each_production_line(&roots, &|_| false, &mut |path, code, _raw| {
            lines.push((normalized(path), code.to_string()));
        });
        lines
    }

    /// Lines of one file, by forward-slash suffix.
    fn lines_of(file_suffix: &str) -> Vec<String> {
        production_source()
            .into_iter()
            .filter(|(path, _)| path.ends_with(file_suffix))
            .map(|(_, code)| code)
            .collect()
    }

    /// **The deletion gate.** `terminal_text` is gone from production
    /// source — not renamed, not `#[allow(dead_code)]`, gone.
    #[test]
    fn terminal_text_no_longer_exists() {
        let source = production_source();
        assert!(
            source.len() > 10_000,
            "the scanner visited only {} production lines — it is not \
             reading the workspace, so 'not found' means nothing",
            source.len()
        );

        let survivors: Vec<String> = source
            .iter()
            .filter(|(_, code)| code.contains("terminal_text"))
            .map(|(path, code)| format!("{path}: {}", code.trim()))
            .collect();
        assert!(
            survivors.is_empty(),
            "`terminal_text` survives in production source — C0a's deletion \
             gate is that the semantic type no longer renders:\n{}",
            survivors.join("\n")
        );
    }

    /// **Anti-vacuous twin for the deletion gate.** The same machinery,
    /// pointed at a seeded definition, must find it. A scanner that cannot
    /// see the symbol it is looking for reports every codebase clean.
    #[test]
    fn the_deletion_scan_would_notice_a_resurrected_terminal_text() {
        let root = tempfile::tempdir().unwrap();
        let src = root.path().join("alpha/src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            root.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"alpha\"]\n",
        )
        .unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "impl Q {\n    pub fn terminal_text(&self) -> String { String::new() }\n}\n",
        )
        .unwrap();

        let mut hits = 0usize;
        common::for_each_production_line(
            &common::production_roots(root.path()),
            &|_| false,
            &mut |_, code, _| {
                if code.contains("terminal_text") {
                    hits += 1;
                }
            },
        );
        assert_eq!(hits, 1, "the scanner missed a seeded `terminal_text`");
    }

    /// **The terminal reaches the NEW renderer, from the site that reads
    /// the answer.**
    ///
    /// Anchored to named function bodies rather than counted per file, for
    /// B0a's reason: a bare "`plain::render` is called somewhere" check
    /// passes green while the read site quietly keeps rendering through the
    /// adapter. `prompt_permission_choice` is the ONE production site that
    /// turns a definition into bytes for an operator, so that is where the
    /// call must be.
    #[test]
    fn the_terminal_reaches_the_new_renderer_from_every_ask_site() {
        let lines = lines_of("newt-tui/src/permissions.rs");
        assert!(
            !lines.is_empty(),
            "the scanner found no production lines in permissions.rs"
        );

        let body = super::b0a::function_body(&lines, "prompt_permission_choice")
            .expect("`fn prompt_permission_choice` not found at all");
        assert!(
            body.contains("plain::render("),
            "the terminal read site does not render through \
             `newt_core::markup::plain`: {body}"
        );

        // ...and nothing in this file renders through the old path any
        // more. The adapter survives for DECODING (`Question::parse` stays
        // the one parser, and BHV-PROMPT-001's Lean theorems govern it) and
        // for the WEB's model reconstruction, which C3 owns — but no
        // production line here turns a form into display bytes.
        let renders_via_adapter: Vec<&String> = lines
            .iter()
            .filter(|code| code.contains("terminal_text"))
            .collect();
        assert!(
            renders_via_adapter.is_empty(),
            "a production line still renders through the semantic type: \
             {renders_via_adapter:#?}"
        );
    }

    /// **Anti-vacuous twin for the reach guard.** Feed it a
    /// `prompt_permission_choice` that never switched: the guard must name
    /// it. And feed it one that did: the guard must be satisfied — so it is
    /// discriminating rather than failing on everything.
    #[test]
    fn the_reach_guard_notices_an_unswitched_read_site() {
        let unswitched: Vec<String> = [
            "fn prompt_permission_choice(w: &PromptWindow, q: &Question<PromptChoice>) -> PromptChoice {",
            "    let prompt = format!(\"{}\\n{MODAL_INPUT_GLYPH}\", q.terminal_text());",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let body =
            super::b0a::function_body(&unswitched, "prompt_permission_choice").expect("body");
        assert!(
            !body.contains("plain::render("),
            "the twin's fixture is already switched, so it proves nothing"
        );

        let switched: Vec<String> = [
            "fn prompt_permission_choice(w: &PromptWindow, d: &InteractionDefinition) -> PromptChoice {",
            "    let prompt = format!(\"{}\\n{MODAL_INPUT_GLYPH}\", plain::render(d));",
            "}",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let body = super::b0a::function_body(&switched, "prompt_permission_choice").expect("body");
        assert!(
            body.contains("plain::render("),
            "the guard rejects a correctly switched read site"
        );
        assert!(
            !body.contains("terminal_text"),
            "the switched fixture still names the deleted method"
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

/// **The B0 deletion gate** (B0b-2, #1846).
///
/// #1803 says B0 is not complete while both permanent paths remain live.
/// These tests are what make that statement checkable.
mod b0b2 {
    use super::common;

    /// The five store methods the old transport exposed. None may exist in
    /// production source any more — not renamed, not `#[allow(dead_code)]`,
    /// gone.
    const DELETED_METHODS: &[&str] = &[
        "publish_permission_question",
        "pending_permission_request",
        "answer_permission_action",
        "take_permission_decision",
        "resolve_permission_request",
        "PendingPermission",
    ];

    /// SQL that USES the old table. Prose that merely names it is fine and
    /// deliberately allowed — the comments explaining what replaced what
    /// are worth keeping, and a gate that banned the NAME would push them
    /// out of the code that needs them.
    const TABLE_USES: &[&str] = &[
        "FROM permission_requests",
        "INTO permission_requests",
        "UPDATE permission_requests",
        "TABLE permission_requests",
        "permission_requests SET",
    ];

    /// Scan for `needles`, choosing the right view of each line.
    ///
    /// `in_strings` picks the RAW line rather than the scanner's `code`,
    /// and that distinction is load bearing rather than defensive: the
    /// shared scanner BLANKS string literals, and SQL lives inside string
    /// literals. Checking SQL against `code` finds nothing, forever —
    /// which is exactly what `the_deletion_scan_sees_a_seeded_use` caught
    /// when this gate was first written against `code` for both classes.
    fn offenders(needles: &[&str], in_strings: bool) -> Vec<String> {
        let roots = common::production_roots(&common::workspace_root());
        let mut found = Vec::new();
        common::for_each_production_line(&roots, &|_| false, &mut |path, code, raw| {
            let haystack = if in_strings { raw } else { code };
            for needle in needles {
                if haystack.contains(needle) {
                    // Normalized per `rel()` so the message reads the same
                    // on every platform (B0a lost a CI round to native
                    // separators leaking into a comparison).
                    let shown = path.to_string_lossy().replace('\\', "/");
                    found.push(format!("{shown}: {}", raw.trim()));
                }
            }
        });
        found
    }

    /// **The deliverable.** After B0b-2 nothing in production reads, writes,
    /// or names the old transport's API.
    #[test]
    fn no_production_path_reads_permission_requests() {
        let methods = offenders(DELETED_METHODS, false);
        assert!(
            methods.is_empty(),
            "the old permission transport still has production callers, so \
             both paths are live and B0 is not complete:\n{}",
            methods.join("\n")
        );
        let sql = offenders(TABLE_USES, true);
        assert!(
            sql.is_empty(),
            "production SQL still uses the `permission_requests` table:\n{}",
            sql.join("\n")
        );
    }

    /// **Anti-vacuous twin.** A scanner that sees nothing reports "deleted"
    /// forever, which is indistinguishable from the state the gate wants.
    /// Seed each needle class into a synthetic workspace and require the
    /// same machinery to find it.
    #[test]
    fn the_deletion_scan_sees_a_seeded_use() {
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
            "fn f() {\n    let _ = store.take_permission_decision(&c, &r);\n                 let _ = conn.execute(\"DELETE FROM permission_requests\");\n}\n",
        )
        .unwrap();

        let mut method_hits = 0usize;
        let mut sql_hits = 0usize;
        common::for_each_production_line(
            &common::production_roots(root.path()),
            &|_| false,
            &mut |_, code, raw| {
                if DELETED_METHODS.iter().any(|n| code.contains(n)) {
                    method_hits += 1;
                }
                // Same view the real gate uses: SQL is inside a string
                // literal, which `code` blanks.
                if TABLE_USES.iter().any(|n| raw.contains(n)) {
                    sql_hits += 1;
                }
            },
        );
        assert_eq!(method_hits, 1, "the scan missed a seeded method call");
        assert_eq!(sql_hits, 1, "the scan missed a seeded SQL use");
    }

    /// The prose that explains the replacement is deliberately still
    /// allowed, so the gate above is not quietly banning documentation.
    #[test]
    fn naming_the_old_table_in_prose_is_not_a_use() {
        let explanatory = "         -- Replaces `permission_requests` as the transport.";
        assert!(!TABLE_USES.iter().any(|n| explanatory.contains(n)));
        assert!(!DELETED_METHODS.iter().any(|n| explanatory.contains(n)));
    }
}
