use super::*;

/// The prose a model sends WITH a tool call becomes one readable row: first
/// sentence, think blocks gone, the call text itself never echoed.
#[test]
fn narration_keeps_the_first_readable_sentence_and_drops_the_call() {
    let cases: &[(&str, Option<&str>)] = &[
        // Native call, plain prose: the sentence, verbatim.
        ("Let me look at the file.", Some("Let me look at the file.")),
        // Two sentences: only the first.
        (
            "First I check the tests. Then I edit.",
            Some("First I check the tests."),
        ),
        // Recovered bare-JSON call: nothing readable.
        (r#"{"name":"read_file","arguments":{"path":"a.rs"}}"#, None),
        // Prose, blank line, fenced JSON call.
        (
            "I'll read the file.\n\n```json\n{\"name\":\"read_file\"}\n```",
            Some("I'll read the file."),
        ),
        // Prose then JSON in the SAME paragraph: the call is cut, not joined.
        (
            "Reading it now:\n{\n  \"name\": \"read_file\",\n  \"arguments\": {}\n}",
            Some("Reading it now:"),
        ),
        // Paired think block, then prose, then a root-tag call.
        (
            "<think>plan</think>\n\nLet me read that now.\n\n<read_file><path>x</path></read_file>",
            Some("Let me read that now."),
        ),
        // Unterminated think: the tail is reasoning, the head is prose.
        ("Reading now<think>cut off mid-thought", Some("Reading now")),
        // Function-tag dialect with a stray closer (the observed qwen3 shape).
        (
            "First I check the tests. Then I edit.\n\n<function=run_command>\n<parameter=command>ls</parameter>\n</function>\n</tool_call>",
            Some("First I check the tests."),
        ),
        // Heading and list markers are structure, not narration.
        (
            "## Plan\n1. Read the config then edit.\n2. Run the tests.",
            Some("Read the config then edit."),
        ),
        ("- extract denials.rs\n- extract find_tool.rs", Some("extract denials.rs")),
        // A dotted token does not end the sentence.
        (
            "Opening newt-core/src/lib.rs to find the seam. Then edit.",
            Some("Opening newt-core/src/lib.rs to find the seam."),
        ),
        // A call glued to the prose on ONE line is cut where it starts: the
        // ⚙ header already shows the arguments.
        (
            "Here is config: {\"debug\": true}",
            Some("Here is config:"),
        ),
        (
            "Reading it now: {\"name\": \"read_file\", \"arguments\": {\"path\": \"a.rs\"}}",
            Some("Reading it now:"),
        ),
        (
            "Opening the file <function=read_file><parameter=path>a.rs</parameter></function>",
            Some("Opening the file"),
        ),
        // Nothing readable.
        ("", None),
        ("   \n\n", None),
        ("</tool_call>", None),
        ("```\nls -la\n```", None),
    ];
    for (content, expected) in cases {
        assert_eq!(
            tool_round_narration(content, 80).as_deref(),
            *expected,
            "content: {content:?}"
        );
    }
}

/// The row is fitted to the caller's width with the shared `…` treatment and
/// never exceeds it.
#[test]
fn narration_is_fitted_to_the_column_budget() {
    let long = "x".repeat(120);
    let row = tool_round_narration(&long, 75).expect("prose");
    assert!(row.ends_with('…'), "{row}");
    assert!(crate::tty::width::str_width(&row) <= 75, "{row}");
    // Under the budget: untouched, no ellipsis.
    assert_eq!(
        tool_round_narration("Short.", 75).as_deref(),
        Some("Short.")
    );
}

/// Whitespace collapses to one row: a multi-line paragraph is still one
/// sentence, and runs of spaces do not survive.
#[test]
fn narration_collapses_whitespace_into_one_row() {
    assert_eq!(
        tool_round_narration("Reading   the\n  config\tfile now.\nThen more.", 80).as_deref(),
        Some("Reading the config file now.")
    );
}
