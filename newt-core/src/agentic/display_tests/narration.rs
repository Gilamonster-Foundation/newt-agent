use super::*;

fn rows(content: &str) -> Vec<String> {
    tool_round_prose(content)
}

/// The prose a model sends WITH a tool call becomes readable rows, one per
/// paragraph or list item: think blocks gone, the call text itself never
/// echoed, nothing cut short.
#[test]
fn narration_keeps_the_readable_prose_and_drops_the_call() {
    let cases: &[(&str, &[&str])] = &[
        // Native call, plain prose: the sentence, verbatim.
        ("Let me look at the file.", &["Let me look at the file."]),
        // Two sentences stay together; nothing is cut at a period.
        (
            "First I check the tests. Then I edit.",
            &["First I check the tests. Then I edit."],
        ),
        (
            "The crates are at the top level (e.g. newt-core, newt-tui). Next I read.",
            &["The crates are at the top level (e.g. newt-core, newt-tui). Next I read."],
        ),
        // Two paragraphs are two rows.
        ("Para one.\n\nPara two.", &["Para one.", "Para two."]),
        // Recovered bare-JSON call: nothing readable.
        (r#"{"name":"read_file","arguments":{"path":"a.rs"}}"#, &[]),
        // Prose, blank line, fenced JSON call.
        (
            "I'll read the file.\n\n```json\n{\"name\":\"read_file\"}\n```",
            &["I'll read the file."],
        ),
        // Prose then JSON in the SAME paragraph: the call is cut, not joined.
        (
            "Reading it now:\n{\n  \"name\": \"read_file\",\n  \"arguments\": {}\n}",
            &["Reading it now:"],
        ),
        // Paired think block, then prose, then a root-tag call.
        (
            "<think>plan</think>\n\nLet me read that now.\n\n<read_file><path>x</path></read_file>",
            &["Let me read that now."],
        ),
        // Unterminated think: the tail is reasoning, the head is prose.
        ("Reading now<think>cut off mid-thought", &["Reading now"]),
        // Function-tag dialect with a stray closer (the observed qwen3 shape).
        (
            "First I check the tests. Then I edit.\n\n<function=run_command>\n<parameter=command>ls</parameter>\n</function>\n</tool_call>",
            &["First I check the tests. Then I edit."],
        ),
        // Heading lines are structure; list items are rows of their own.
        (
            "## Plan\n1. Read the config then edit.\n2. Run the tests.",
            &["Read the config then edit.", "Run the tests."],
        ),
        (
            "- extract denials.rs\n- extract find_tool.rs",
            &["extract denials.rs", "extract find_tool.rs"],
        ),
        // A call glued to the prose on ONE line is cut where it starts: the
        // ⚙ header already shows the arguments.
        ("Here is config: {\"debug\": true}", &["Here is config:"]),
        (
            "Reading it now: {\"name\": \"read_file\", \"arguments\": {\"path\": \"a.rs\"}}",
            &["Reading it now:"],
        ),
        (
            "Opening the file <function=read_file><parameter=path>a.rs</parameter></function>",
            &["Opening the file"],
        ),
        // Nothing readable.
        ("", &[]),
        ("   \n\n", &[]),
        ("</tool_call>", &[]),
        ("```\nls -la\n```", &[]),
    ];
    for (content, expected) in cases {
        assert_eq!(rows(content), *expected, "content: {content:?}");
    }
}

/// Whitespace collapses within a row: a soft-wrapped paragraph joins back
/// together, and runs of spaces do not survive.
#[test]
fn narration_collapses_whitespace_within_a_row() {
    assert_eq!(
        rows("Reading   the\n  config\tfile now.\nThen more."),
        vec!["Reading the config file now. Then more."]
    );
}
