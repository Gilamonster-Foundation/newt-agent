use super::*;

#[test]
fn tool_call_lines_wrap_without_losing_the_command() {
    assert_eq!(
        tool_call_lines("find", ". (name=*.rs, type=f)", 80),
        vec!["⚙  find: . (name=*.rs, type=f)"]
    );
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
