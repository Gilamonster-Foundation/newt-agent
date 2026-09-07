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
