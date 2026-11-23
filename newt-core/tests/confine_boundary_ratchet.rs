//! Code that has to build without agent-bridle does not name it (#3160).
//!
//! newt-core's `confine` feature is to gate every use of agent-bridle. The files listed
//! here stay compiled with `confine` off, tests included, since CI's confine-off job
//! builds `--all-targets`. So no line of theirs may name an agent-bridle crate outside a
//! comment; code that needs bridle, a test included, lives in a file that is not listed.
//! `confined_exec.rs` holds the request, result and refusal types its callers use; the
//! executor that uses bridle is its child `confined_exec/executor.rs`, which is not
//! listed.
//!
//! The list only grows: each later step of #3160 adds the files it frees of bridle, and
//! the step that makes the bridle dependencies optional deletes this file, because from
//! then on the compiler is the proof. The files are embedded with `include_str!`, so
//! nothing here does I/O, and renaming a listed file fails to compile instead of dropping
//! it from the scan.

/// The files that name no agent-bridle crate outside a comment, by repository path.
const BRIDLE_FREE: &[(&str, &str)] = &[(
    "newt-core/src/confined_exec.rs",
    include_str!("../src/confined_exec.rs"),
)];

/// The crates' Rust names (`agent_bridle`, `agent_bridle_tool_shell`, ...) and their
/// package names, as a `#[path]` into a vendored copy would spell them.
const FORBIDDEN: &[&str] = &["agent_bridle", "agent-bridle"];

/// `line` up to its `//` comment, if it has one. The line is cut at its first `//` only
/// when it holds no `"` and no `*/` anywhere, no `/*` comes before the `//`, and it does
/// not end in `\`. Then no string or block comment opens before the `//` or ends on the
/// line, and the line does not continue a string, so the cut text is a comment or the
/// text of a string that opened on an earlier line and holds a raw line break. A path in
/// `#[path]` or `include!` holds one only if a file name does. Beyond that the cut errs
/// only toward reporting: it reads a `/* */` comment, and a `//` comment on a line that holds
/// a `"` or `*/` or ends in `\`, as code.
fn code_part(line: &str) -> &str {
    match line.find("//") {
        Some(at)
            if !line.contains('"')
                && !line.contains("*/")
                && !line[..at].contains("/*")
                && !line.ends_with('\\') =>
        {
            &line[..at]
        }
        _ => line,
    }
}

/// The numbers of the lines of `src` that name an agent-bridle crate outside a comment. A
/// name counts when one line spells it whole, so one split by a `\` continuation, written
/// with an escape or built with `concat!` is not seen; each of those is deliberate, and
/// the compiler is the proof once the bridle dependencies are optional.
fn naming_bridle(src: &str) -> Vec<usize> {
    src.lines()
        .enumerate()
        .filter(|(_, line)| FORBIDDEN.iter().any(|name| code_part(line).contains(name)))
        .map(|(index, _)| index + 1)
        .collect()
}

#[test]
fn code_that_builds_without_confine_names_no_agent_bridle_crate() {
    let mut found = Vec::new();
    for (path, src) in BRIDLE_FREE {
        let lines: Vec<&str> = src.lines().collect();
        for number in naming_bridle(src) {
            found.push(format!("{path}:{number}: {}", lines[number - 1].trim()));
        }
    }
    assert!(
        found.is_empty(),
        "these lines name agent-bridle in code that has to build without it; move them \
         into a file that is not listed: {found:#?}"
    );
}

/// Anti-vacuous twin: the scan reports each way of naming bridle in code, test code
/// included, and passes over `//` comments.
#[test]
fn the_scan_reads_every_line_outside_a_comment() {
    assert_eq!(naming_bridle("use agent_bridle::Gate;\n"), [1]);
    assert_eq!(
        naming_bridle("fn f(_: agent_bridle_tool_shell::ShellTool) {}\n"),
        [1]
    );
    assert_eq!(
        naming_bridle("#[path = \"../vendor/agent-bridle-core/src/lib.rs\"]\nmod core;\n"),
        [1]
    );
    // Test code is built with `confine` off too.
    assert_eq!(
        naming_bridle("#[cfg(test)]\nmod tests {\n    use agent_bridle::Gate;\n}\n"),
        [3]
    );
    let comments = "// agent_bridle\n    /// [`agent_bridle::Gate`]\n//! agent-bridle\n\
                    fn f() {} // agent_bridle\n";
    assert!(naming_bridle(comments).is_empty());
    // A `//` after a `"` or a `/*` is not taken to start a comment, and neither is one
    // in a string or block comment that opened on an earlier line and ends on this one,
    // nor the end of such a comment followed by a division (`*//`).
    assert_eq!(
        naming_bridle("call(\"http://x\", agent_bridle::G);\nf(/* // */ agent_bridle::G);\n"),
        [1, 2]
    );
    let match_arm = "fn f(s: &str) -> u8 {\n    match s {\n        \"a\nhttp://b\" => \
                     agent_bridle::N,\n        _ => 0,\n    }\n}\n";
    assert_eq!(naming_bridle(match_arm), [4]);
    let struct_field = "struct S {\n    /* see\n    http://b */ g: agent_bridle::G,\n}\n";
    assert_eq!(naming_bridle(struct_field), [3]);
    let divided = "fn f() -> u8 {\n    8 /* see\n    *// agent_bridle::N\n}\n";
    assert_eq!(naming_bridle(divided), [3]);
    // A path continued with `\` keeps its `//` line whole.
    let continued = "#[path = \"../../vendor/\\\n          //agent-bridle-core/src/\\\n\
                     lib.rs\"]\nmod core;\n";
    assert_eq!(naming_bridle(continued), [2]);
}
