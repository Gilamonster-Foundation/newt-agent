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

/// `line` up to its `//` comment, if it has one. A `//` starts a comment here only when
/// no `"` or `/*` comes before it on the line, so a `//` inside a string or a block
/// comment leaves the whole line to be read: the scan errs toward reporting. It reads one
/// shape wrongly: inside a multi-line string, a line that starts with `//` is taken for a
/// comment, so code written after the string closes on that line is not read.
fn code_part(line: &str) -> &str {
    match line.find("//") {
        Some(at) if !line[..at].contains('"') && !line[..at].contains("/*") => &line[..at],
        _ => line,
    }
}

/// The numbers of the lines of `src` that name an agent-bridle crate outside a comment.
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
/// included, and passes over comments.
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
    // A `//` after a `"` or a `/*` is not taken to start a comment.
    assert_eq!(
        naming_bridle("call(\"http://x\", agent_bridle::G);\nf(/* // */ agent_bridle::G);\n"),
        [1, 2]
    );
}
