use super::*;

#[test]
fn explain_and_research_receive_their_intended_bounded_tool_loops() {
    let explain = PromptIntake::analyze("Explain how prompt receipts survive compaction.");
    let research = PromptIntake::analyze("Investigate the current compaction behavior.");

    assert_eq!(explain.disposition(), PromptDisposition::Explain);
    assert_eq!(research.disposition(), PromptDisposition::Research);
    assert_eq!(PromptDisposition::Ask.tool_round_limit(40), 0);
    assert_eq!(PromptDisposition::Explain.tool_round_limit(40), 40);
    assert_eq!(PromptDisposition::Research.tool_round_limit(40), 3);
    assert_eq!(PromptDisposition::Plan.tool_round_limit(40), 40);
}

// ── #1260: disposition inference as pure data ───────────────────────────

/// The pre-#1260 lists, reconstructed as an override — documents the cliff
/// durably: under the OLD data this prompt matched NOTHING ("what is" ≠
/// "what are"; research had "find out", not "largest") and was classified
/// Explain SOLELY by the trailing `?`, while the identical prompt minus its
/// `?` fell to Act. Any future change to this coupling is now deliberate.
fn pre_1260_lexicon() -> DispositionLexicon {
    DispositionLexicon {
        action: [
            "implement",
            "modify",
            "change",
            "create",
            "write",
            "edit",
            "delete",
            "fix",
            "build",
            "run ",
            "execute",
            "commit",
            "push",
            "open a pr",
            "open pr",
            "merge",
        ]
        .map(str::to_string)
        .to_vec(),
        research: [
            "research",
            "investigate",
            "look up",
            "find out",
            "analyze",
            "diagnose",
            "audit",
            "explore",
            "compare",
        ]
        .map(str::to_string)
        .to_vec(),
        explain: [
            "explain",
            "summarize",
            "describe",
            "what is",
            "why ",
            "how does",
            "how do",
        ]
        .map(str::to_string)
        .to_vec(),
        question_mark_disposition: PromptDisposition::Explain,
        // #1971's lists are not part of the reconstructed 2026-era data;
        // taking the defaults keeps this fixture about the #1260 cliff.
        ..DispositionLexicon::default()
    }
}

/// `infer_disposition_with` over a prompt's OWN extracted asks — the shape
/// production uses. #1971 gave the classifier a second input; these tests
/// still measure the same thing through it.
fn infer(prompt: &str, lexicon: &DispositionLexicon) -> PromptDisposition {
    let (asks, _) = super::extract_atomic_asks_with(prompt, lexicon);
    super::infer_disposition_with(prompt, &asks, lexicon)
}

#[test]
fn largest_files_question_classified_explain_via_question_mark_fallback_pre_1260() {
    let old = pre_1260_lexicon();
    assert_eq!(
        infer(LARGEST_FILES_PROMPT, &old),
        PromptDisposition::Explain,
        "under the OLD data the ? fallback alone decided"
    );
    assert_eq!(
        infer(LARGEST_FILES_PROMPT.trim_end_matches('?'), &old),
        PromptDisposition::Act,
        "…and the same prompt minus its ? fell off the cliff to Act"
    );
}

#[test]
fn new_defaults_classify_evidence_questions_by_content_not_the_cliff() {
    // "largest" (research data) decides — with or without the `?`.
    let with_q = PromptIntake::analyze(LARGEST_FILES_PROMPT);
    assert_eq!(with_q.disposition(), PromptDisposition::Research);
    let without_q = PromptIntake::analyze("What are the 10 largest Rust files in this workspace");
    assert_eq!(
        without_q.disposition(),
        PromptDisposition::Research,
        "content decides; removing the ? no longer flips the disposition"
    );
    // "what are" (explain data) catches the plural interrogative the old
    // list missed.
    let plural = PromptIntake::analyze("What are the tradeoffs of this design?");
    assert_eq!(plural.disposition(), PromptDisposition::Explain);
    // A bare statement matching nothing still defaults to Act.
    let act = PromptIntake::analyze("update the release notes for 0.8.0");
    assert_eq!(act.disposition(), PromptDisposition::Act);
}

#[test]
fn line_count_questions_classify_research_not_the_cliff() {
    // #1387: the regressed prompt. "line count" is evidence phrasing, so it
    // lands in Research — where `find` (sort=lines/show_lines) can answer it
    // read-only. It must NOT fall off the `?` cliff to Explain, and must NOT
    // require Act (a mutation grant) just to count lines.
    let regressed =
        PromptIntake::analyze("show me the 10 code files with the highest line counts?");
    assert_eq!(
        regressed.disposition(),
        PromptDisposition::Research,
        "line-count question is a Research/evidence turn, not Explain or Act"
    );
    for prompt in [
        "which files have the most lines",
        "the longest file in the repo",
        "files with the fewest lines",
    ] {
        assert_eq!(
            PromptIntake::analyze(prompt).disposition(),
            PromptDisposition::Research,
            "line-count evidence phrasing → Research: {prompt:?}"
        );
    }
}

#[test]
fn code_file_prompt_adds_source_scope_without_incident_specific_shape_guessing() {
    let intake = PromptIntake::analyze(
        "show me the 10 code files with the highest line counts in this repository?",
    );
    let card = intake.model_card();

    assert!(
        !card.contains("response_shape:"),
        "line-count/ranking keywords must not own presentation policy: {card}"
    );
    assert!(
        card.contains("evidence_scope: source_files"),
        "`code files` means language source, not every repository file: {card}"
    );
    assert!(
        card.contains("source_filter: category=source"),
        "an unqualified code-file request must use the harness-owned source category: {card}"
    );
    assert!(
        card.contains("exclude documentation, manifests, lockfiles"),
        "the steering must name the observed false-positive classes: {card}"
    );
    assert!(
        !card.contains("highest")
            && !card.contains("longest")
            && !card.contains("most lines")
            && !card.contains("line/size rankings")
            && !card.contains("code=true"),
        "the model card must carry a general source refinement, not an incident lexicon: {card}"
    );
}

#[test]
fn explicit_rust_table_prompt_steers_rs_filter_and_gfm_table() {
    let intake = PromptIntake::analyze(
        "can you give me a table of the rust files with the longest line counts instead?",
    );
    let card = intake.model_card();

    assert!(card.contains("response_shape: table"), "{card}");
    assert!(card.contains("evidence_scope: source_files"), "{card}");
    assert!(
        card.contains("source_extensions: rs"),
        "Rust must resolve through the language-pack data to its source extension: {card}"
    );
    assert!(
        card.contains("source_filter: category=source language=rust"),
        "the model needs the concrete harness filter, not just a language label: {card}"
    );
}

#[test]
fn ordinary_prompt_gets_no_incident_specific_refinement() {
    let card = PromptIntake::analyze("explain ownership briefly").model_card();

    assert!(!card.contains("response_format:"), "{card}");
    assert!(!card.contains("response_shape:"), "{card}");
    assert!(!card.contains("evidence_scope:"), "{card}");
    assert!(!card.contains("source_filter:"), "{card}");

    let comfortable = PromptIntake::analyze("make this interface more comfortable").model_card();
    assert!(
        !comfortable.contains("response_shape:"),
        "presentation inference must not match `table` inside another word: {comfortable}"
    );
}

#[test]
fn lexicon_overrides_drive_inference_table_driven() {
    // A dropped-in override list REPLACES its default wholesale.
    let custom = DispositionLexicon {
        explain: vec!["kerfuffle".to_string()],
        question_mark_disposition: PromptDisposition::Research,
        ..DispositionLexicon::default()
    };
    for (prompt, want) in [
        ("tell me about the kerfuffle", PromptDisposition::Explain),
        // The default explain needles are GONE (replaced), so "what is…?"
        // now reaches the retargeted ? fallback → Research.
        ("what is a monad?", PromptDisposition::Research),
        // Action still wins outright.
        ("fix the kerfuffle", PromptDisposition::Act),
        // No needle, no ?: Act.
        ("status report", PromptDisposition::Act),
    ] {
        assert_eq!(infer(prompt, &custom), want, "{prompt:?}");
    }
}

#[test]
fn analyze_with_applies_the_lexicon_and_keeps_ask_precedence() {
    // The lexicon changes the classification vs the defaults…
    let lex = DispositionLexicon {
        research: vec!["kerfuffle".to_string()],
        ..DispositionLexicon::default()
    };
    let intake = PromptIntake::analyze_with("tell me about the kerfuffle", &lex);
    assert_eq!(intake.disposition(), PromptDisposition::Research);
    // …but an unresolved decision still forces the Ask terminal, with the
    // lexicon-derived value preserved as the post-lock disposition.
    let asky = PromptIntake::analyze_with(
        "Investigate either the kerfuffle or the brouhaha; compare them.",
        &lex,
    );
    if asky.manifest().pending_decision_count() > 0 {
        assert_eq!(asky.disposition(), PromptDisposition::Ask);
    }
    // The empty-prompt Ask terminal is untouched by any lexicon.
    let empty = PromptIntake::analyze_with("   ", &lex);
    assert_eq!(empty.disposition(), PromptDisposition::Ask);
}

/// #2332 / #2331 / #2283: a request phrased as a question is a request. These
/// are the recorded prompts; each matched no needle and reached Explain only
/// through the `?` fallback, so an install, a review, and a retry all lost the
/// tools they needed. Each must now route exactly like its imperative form.
#[test]
fn a_request_phrased_as_a_question_routes_like_its_imperative() {
    let mut misrouted = Vec::new();
    for (question, imperative) in [
        (
            "Can you install skills for that herdr tool?",
            "install skills for that herdr tool",
        ),
        // #2332 wrote this one as "change 42", which the `change` action needle
        // already routes to Act; the recorded review reached Explain.
        ("Can you review PR 42 for me?", "review PR 42 for me"),
        (
            "Could you fetch the release notes from the tracker?",
            "fetch the release notes from the tracker",
        ),
        ("try now?", "try now"),
        (
            "Would you look at the failing job?",
            "look at the failing job",
        ),
        // The opener is read per clause, not only at byte zero.
        (
            "Thanks for that. Can you review PR 42?",
            "Thanks for that. review PR 42",
        ),
    ] {
        let lex = DispositionLexicon::default();
        assert_eq!(
            infer(imperative, &lex),
            PromptDisposition::Act,
            "{imperative:?}"
        );
        if infer(question, &lex) != PromptDisposition::Act {
            misrouted.push((question, infer(question, &lex)));
        }
    }
    assert!(
        misrouted.is_empty(),
        "requests routed as answers: {misrouted:?}"
    );
}

/// The twin of the test above, and the vacuous-green trap it closes: adding
/// request words proves nothing unless genuine questions still stay answers.
/// These stay out of `Act`, which also keeps them clear of the Act-only action
/// nudges and self-verify gate (`agentic/mod.rs`, `action_nudges && … == Act`),
/// so moving requests to Act hands no greeting or read-only answer a test
/// obligation (#2324).
#[test]
fn a_genuine_question_stays_an_answer() {
    let lex = DispositionLexicon::default();
    for question in [
        "How do I install a skill?",
        "What does tool_search do?",
        "hello?",
        "What can you do?",
        "Can you explain how prompt intake works?",
        "Could you tell me what the herdr tool does?",
        "Thanks. How do I install a skill?",
    ] {
        assert_eq!(
            infer(question, &lex),
            PromptDisposition::Explain,
            "{question:?} is a question, not a request"
        );
    }
}

/// Recorded, not endorsed: a capability question opening with a request opener
/// now reaches Act, so it enters the Act-only action nudges and self-verify
/// gate. Whether a test is owed belongs to #2324, which decides it from the
/// task rather than the disposition; this fixture hands that lane the case.
#[test]
fn a_capability_question_with_a_request_opener_reaches_act_for_2324() {
    assert_eq!(
        infer("Can you read Python?", &DispositionLexicon::default()),
        PromptDisposition::Act
    );
}

/// "refactor" is an instruction to change code. It was absent from the action
/// list, so "refactor the largest file in this repo" (the operator's standing
/// acceptance prompt) fell through to the research needle "largest" — a
/// read-only, 3-round turn that could never refactor anything (live
/// 2026-09-23). An action needle anywhere wins over a research one.
#[test]
fn refactor_is_an_action_even_when_the_target_is_named_by_size() {
    for prompt in [
        "refactor the largest file in this repo",
        "Refactor newt-core/src/agentic/mod.rs",
    ] {
        assert_eq!(
            PromptIntake::analyze(prompt).disposition(),
            PromptDisposition::Act,
            "{prompt}"
        );
    }
    // The evidence question that motivated "largest" stays Research.
    assert_eq!(
        PromptIntake::analyze("What are the 10 largest Rust files in this workspace").disposition(),
        PromptDisposition::Research
    );
}

/// #2553 finding 3: the action lexicon matched "refactor" by bare substring,
/// so "explain **refactor**ing strategies" hit the action needle before ever
/// reaching the `explain` list — an explanatory prompt granted full mutation
/// authority. The fix is whole-word matching in the lexicon matcher itself
/// (`contains_word`), not a patch scoped to "refactor" alone.
#[test]
fn explain_refactoring_is_not_the_refactor_action_needle() {
    assert_eq!(
        PromptIntake::analyze("explain refactoring strategies").disposition(),
        PromptDisposition::Explain,
        "refactoring is a different word from refactor, in an explanatory sentence"
    );
}

/// The whole-word fix applies to every action needle, not just "refactor":
/// "fix" is a substring of "fixture", and "explain the test fixture setup" is
/// an explanatory prompt, not an instruction to fix anything.
#[test]
fn explain_test_fixture_is_not_the_fix_action_needle() {
    assert_eq!(
        PromptIntake::analyze("explain the test fixture setup").disposition(),
        PromptDisposition::Explain,
        "fixture is a different word from fix"
    );
}

/// The whole-word switch has a real cost, not just a benefit: the research
/// needle "line count" (singular) stopped matching inside "line counts"
/// (plural) once boundary checks applied, regressing
/// `line_count_questions_classify_research_not_the_cliff`. Fixed by adding
/// the plural as its own lexicon entry (data, not a matcher special case) —
/// this test pins that the plural form still classifies Research.
#[test]
fn plural_line_counts_still_classifies_research() {
    assert_eq!(
        PromptIntake::analyze("show me the 10 code files with the highest line counts?")
            .disposition(),
        PromptDisposition::Research,
        "the plural form must match its own lexicon entry, not rely on substring matching"
    );
}

/// #2562 round 2 (PR-2562 review, "Where authority actually changes" table,
/// red first): whole-word matching (round 1) fixed the six regression rows
/// at the bottom of this table — the "intended fix" — but broke authority in
/// BOTH directions for every other row. Each row here was confirmed FAILED
/// against the round-1 commit (`e494925b`) before the round-2 lexicon
/// additions landed:
/// - a read-only gerund GAINED Act (`auditing …` → Act, was Research;
///   `researching …` → Act, was Research; `explaining …` → Act, was Explain)
///   because the research/explain needle no longer matched its own
///   inflection, so the prompt fell through to the terminal Act fallback;
/// - a mixed prompt LOST Act (`… and get it fixed` → Research/Explain, was
///   Act) because its only action cue was an inflected form no bare-verb
///   needle covers by design (round 2's ruling: explicit phrases, not
///   stemming).
///
/// The six "intended fix" rows are pinned here too, so this one table test
/// is the complete authority-boundary regression suite for both rounds.
#[test]
fn where_authority_actually_changes_matches_the_review_table() {
    let cases: &[(&str, PromptDisposition)] = &[
        // Mixed prompts: restored to Act by the round-2 action PHRASES
        // (never bare gerunds — see the lexicon's own comment).
        (
            "investigate the flaky test and get it fixed",
            PromptDisposition::Act,
        ),
        ("the largest file needs refactoring", PromptDisposition::Act),
        (
            "Why does this crash? It needs fixing.",
            PromptDisposition::Act,
        ),
        (
            "investigate why CI failed and get it committed",
            PromptDisposition::Act,
        ),
        ("rerun the tests and investigate", PromptDisposition::Act),
        (
            "The auth module is broken and needs fixing.",
            PromptDisposition::Act,
        ),
        // Read-only gerunds: restored to Research/Explain by the round-2
        // inflection DATA (never gained Act back through the fallback).
        ("auditing the permission table", PromptDisposition::Research),
        (
            "researching which crate to use",
            PromptDisposition::Research,
        ),
        ("explaining the retry loop", PromptDisposition::Explain),
        // The six "intended fix" rows (round 1's whole-word matching) —
        // regression-pinned here so a future change to either round cannot
        // silently re-admit the false Acts round 1 was written to remove.
        ("explain refactoring strategies", PromptDisposition::Explain),
        ("explain the prefix tree", PromptDisposition::Explain),
        ("what is the credit limit field", PromptDisposition::Explain),
        (
            "explain the test fixture layout",
            PromptDisposition::Explain,
        ),
        (
            "describe the emergency stop path",
            PromptDisposition::Explain,
        ),
        ("explain the exchange module", PromptDisposition::Explain),
    ];
    for (prompt, expected) in cases {
        assert_eq!(
            PromptIntake::analyze(prompt).disposition(),
            *expected,
            "{prompt:?} must classify {expected:?}"
        );
    }
}
