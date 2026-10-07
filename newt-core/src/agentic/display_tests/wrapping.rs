use super::*;

#[test]
fn tool_call_lines_wrap_without_losing_the_command() {
    assert_eq!(
        tool_call_lines("find", ". (name=*.rs, type=f)", 80),
        vec!["⚙  find: . (name=*.rs, type=f)"]
    );
}

/// A long or multi-line detail is a block under the name, at the glyph
/// margin, rather than a hanging column; nothing is dropped either way.
#[test]
fn tool_call_lines_render_long_details_as_a_block() {
    let question = "The largest file is mod.rs (11,726 lines). It already declares many \
                    submodules.\n\nHow would you like to split it? My recommendation: pure \
                    code-motion first, then narrow the seam.";
    let lines = tool_call_lines("request_user_input", question, 60);
    assert_eq!(lines[0], "⚙  request_user_input:");
    assert!(lines.len() > 3, "{lines:?}");
    assert!(
        lines[1..].iter().all(|l| l.starts_with("   ")),
        "every detail row sits at the glyph margin: {lines:?}"
    );
    assert!(lines.iter().all(|l| l.chars().count() <= 60), "{lines:?}");
    let rejoined: String = lines[1..]
        .iter()
        .map(|l| l.trim_start_matches("   "))
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(
        rejoined.replace('\n', ""),
        question.replace('\n', ""),
        "the whole question survives"
    );
    // Two hanging rows is still the compact shape.
    let two = tool_call_lines("grep", &"x ".repeat(40), 60);
    assert!(two[0].starts_with("⚙  grep: "), "{two:?}");
    assert_eq!(two.len(), 2, "{two:?}");
}

#[test]
fn wrap_to_width_never_drops_and_hard_splits_long_tokens() {
    // #1153: the full command must survive — reassembling the wrap yields
    // the original (spaces preserved via split_inclusive).
    let cmd = "grep -rn \"NudgerProfile|resolve.*knob|KNOWN_KNOBS\" --include=*.rs newt-core/src newt-cli/src";
    let wrapped = super::wrap_to_width(cmd, 20);
    assert_eq!(wrapped.join(""), cmd, "no characters lost");
    assert!(
        wrapped.iter().all(|l| l.chars().count() <= 20),
        "each line fits"
    );
    assert!(wrapped.len() > 1, "long command actually wraps");

    // A single token longer than the width is hard-split, not dropped.
    let long = "a".repeat(50);
    let w = super::wrap_to_width(&long, 10);
    assert_eq!(w.join(""), long);
    assert_eq!(w.len(), 5);

    // Short input stays one line.
    assert_eq!(super::wrap_to_width("cargo build", 40), vec!["cargo build"]);
    // Embedded newlines split into separate logical lines.
    assert_eq!(super::wrap_to_width("a\nb", 40), vec!["a", "b"]);
}
