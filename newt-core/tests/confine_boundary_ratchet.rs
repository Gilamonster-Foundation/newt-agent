//! Code that has to build without agent-bridle does not name it (#3160).
//!
//! newt-core's `confine` feature is to gate every use of agent-bridle. The files listed
//! here stay compiled with `confine` off, so their production code must not name an
//! agent-bridle crate. `confined_exec.rs` holds the request, result and refusal types
//! its callers use; the executor that uses bridle is its child
//! `confined_exec/executor.rs`, which is not listed. Comments and test-only code may
//! name bridle.
//!
//! The list only grows: each later step of #3160 adds the files it frees of bridle, and
//! the step that makes the bridle dependencies optional deletes this file, because from
//! then on the compiler is the proof. The files are embedded with `include_str!`, so
//! nothing here does I/O, and renaming a listed file fails to compile instead of dropping
//! it from the scan.

/// The files whose production code names no agent-bridle crate, by repository path.
const BRIDLE_FREE: &[(&str, &str)] = &[(
    "newt-core/src/confined_exec.rs",
    include_str!("../src/confined_exec.rs"),
)];

/// The crates' Rust names (`agent_bridle`, `agent_bridle_tool_shell`, ...) and their
/// package names, as a `#[path]` into a vendored copy would spell them.
const FORBIDDEN: &[&str] = &["agent_bridle", "agent-bridle"];

enum State {
    Code,
    /// After a test-only attribute, before the header of the item it gates.
    TestAttributes,
    /// Inside a test-only item.
    TestItem,
}

/// `line` without a trailing `//` comment, trimmed.
fn code_part(line: &str) -> &str {
    line.split_once("//").map_or(line, |(code, _)| code).trim()
}

/// A column-0 attribute that compiles what follows only for tests.
fn is_test_attribute(line: &str) -> bool {
    line.starts_with("#[cfg(test)]")
        || line.starts_with("#[cfg(all(test,")
        || line.starts_with("#[cfg(all(test)")
}

/// An item header that is the whole item: `mod tests;`, `fn f() {}`.
fn ends_item(header: &str) -> bool {
    let code = code_part(header);
    code.ends_with(';') || code.ends_with('}')
}

/// The production lines of `src`, as `(line number, line)`: every line outside a
/// test-only top-level item, less whole-line comments.
///
/// A test-only item starts at a column-0 `#[cfg(test)]` or `#[cfg(all(test, ..))]`. Its
/// header is the next line that is not an attribute or a comment; the item ends there if
/// the header ends in `;` or `}`, else at the next column-0 line that starts with `}`, `)`
/// or `]`, which in rustfmt's layout (CI checks it) is the item's closing line. Any other
/// column-0 line inside the item, such as a string literal's continuation, does not end
/// it. A test attribute anywhere else is not recognised, so the code under it is read: the
/// scan errs toward reporting.
fn production_lines(src: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut state = State::Code;
    for (index, line) in src.lines().enumerate() {
        let trimmed = line.trim();
        state = match state {
            State::TestItem if line.starts_with(['}', ')', ']']) => State::Code,
            State::TestItem => State::TestItem,
            State::TestAttributes
                if trimmed.is_empty() || trimmed.starts_with("#[") || trimmed.starts_with("//") =>
            {
                State::TestAttributes
            }
            State::TestAttributes if ends_item(trimmed) => State::Code,
            State::TestAttributes => State::TestItem,
            State::Code if is_test_attribute(line) => {
                // The item can share the attribute's line: `#[cfg(test)] mod t;`.
                let rest = line.split_once(']').map_or("", |(_, rest)| code_part(rest));
                if rest.is_empty() {
                    State::TestAttributes
                } else if ends_item(rest) {
                    State::Code
                } else {
                    State::TestItem
                }
            }
            State::Code => {
                if !trimmed.starts_with("//") {
                    lines.push((index + 1, line));
                }
                State::Code
            }
        };
    }
    lines
}

/// The numbers of the production lines of `src` that name an agent-bridle crate.
fn naming_bridle(src: &str) -> Vec<usize> {
    production_lines(src)
        .into_iter()
        .filter(|(_, line)| FORBIDDEN.iter().any(|name| code_part(line).contains(name)))
        .map(|(number, _)| number)
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
         into a confine-gated file or test-only code: {found:#?}"
    );
}

/// Anti-vacuous twin: on each listed file the scan reads no test as production code and
/// still reads a line appended after the file's tests; on small sources it reports each
/// way of naming bridle, including code after a test module, and ignores comments and
/// test-only code.
#[test]
fn the_scan_reads_each_production_line_and_no_test_code() {
    for (path, src) in BRIDLE_FREE {
        assert!(
            !production_lines(src)
                .iter()
                .any(|(_, line)| line.trim() == "#[test]"),
            "{path}: a test was read as production code"
        );
        let appended = format!("{src}use agent_bridle::Gate;\n");
        assert_eq!(
            naming_bridle(&appended).last(),
            Some(&(src.lines().count() + 1)),
            "{path}: a bridle import appended after the tests was not read"
        );
    }

    assert_eq!(naming_bridle("use agent_bridle::Gate;\n"), [1]);
    assert_eq!(
        naming_bridle("fn f(_: agent_bridle_tool_shell::ShellTool) {}\n"),
        [1]
    );
    assert_eq!(
        naming_bridle("#[path = \"../vendor/agent-bridle-core/src/lib.rs\"]\nmod core;\n"),
        [1]
    );
    let comments = "// agent_bridle\n    /// [`agent_bridle::Gate`]\n//! agent-bridle\n\
                    fn f() {} // agent_bridle\n";
    assert!(naming_bridle(comments).is_empty());
    let after_tests = "fn f() {}\n\
                       #[cfg(test)]\nmod tests {\n    use agent_bridle::Gate;\n}\n\
                       #[cfg(all(test, target_os = \"linux\"))]\n#[path = \"t.rs\"]\nmod t;\n\
                       #[cfg(test)] mod u { use agent_bridle::A; }\n\
                       use agent_bridle::Gate;\n";
    assert_eq!(naming_bridle(after_tests), [10]);
    // A string literal that continues at column 0 does not end the test module.
    let column_zero_string = "#[cfg(test)]\nmod tests {\n    const S: &str = \"a\nb\";\n    \
                              use agent_bridle::Gate;\n}\nuse agent_bridle::Gate;\n";
    assert_eq!(naming_bridle(column_zero_string), [7]);
    assert_eq!(
        naming_bridle("#[cfg(any(test, unix))]\nuse agent_bridle::Gate;\n"),
        [2]
    );
    assert_eq!(
        naming_bridle("#[cfg(not(test))]\nuse agent_bridle::Gate;\n"),
        [2]
    );
    assert_eq!(
        naming_bridle("#[cfg(all(test_support, unix))]\nuse agent_bridle::Gate;\n"),
        [2]
    );
}
