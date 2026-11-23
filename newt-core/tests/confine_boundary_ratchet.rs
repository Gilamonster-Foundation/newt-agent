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

/// A code part that leaves a string literal open: an odd number of unescaped `"`, as in
/// `const J: &str = r#"{`.
fn opens_string(code: &str) -> bool {
    let mut quotes = 0;
    let mut escaped = false;
    for c in code.chars() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            quotes += 1;
        }
    }
    quotes % 2 == 1
}

/// A header whose item rustfmt closes with a column-0 `}`: it ends in `{`, outside a
/// string literal.
fn opens_block(header: &str) -> bool {
    let code = code_part(header);
    code.ends_with('{') && !opens_string(code)
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
/// header is the next line that is not an attribute or a comment. The item ends there if
/// the header ends in `;` or `}`. If the header ends in `{` outside a string literal, the
/// item is skipped up to the next column-0 line that starts with `}`, which in rustfmt's
/// layout (CI checks it) is the item's closing line. Any other header (a `const` broken
/// after `=`, a string fixture, a multi-line signature or attribute) is read as production
/// code, with everything after it, and so is the code under a test attribute that is not
/// at column 0: the scan errs toward reporting.
///
/// The scan reads lines, not tokens, so it has limits:
/// - a column-0 `}` inside a skipped item (a string fixture that closes at column 0, a
///   `} else {`) ends the skip early and the rest of the item is read; that fails loudly,
///   since the twin's `#[test]` check fails on any listed file where it happens;
/// - column-0 `#[cfg(test)]` text inside a production string is taken for an attribute;
/// - `code_part` cuts a line at its first `//`, even inside a string.
fn production_lines(src: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut state = State::Code;
    for (index, line) in src.lines().enumerate() {
        let trimmed = line.trim();
        state = match state {
            State::TestItem if line.starts_with('}') => State::Code,
            State::TestItem => State::TestItem,
            State::TestAttributes
                if trimmed.is_empty() || trimmed.starts_with("#[") || trimmed.starts_with("//") =>
            {
                State::TestAttributes
            }
            State::TestAttributes if ends_item(trimmed) => State::Code,
            State::TestAttributes if opens_block(trimmed) => State::TestItem,
            State::TestAttributes => {
                lines.push((index + 1, line));
                State::Code
            }
            State::Code if is_test_attribute(line) => {
                // The item can share the attribute's line: `#[cfg(test)] mod t;`.
                let rest = line.split_once(']').map_or("", |(_, rest)| code_part(rest));
                if rest.is_empty() {
                    State::TestAttributes
                } else if ends_item(rest) {
                    State::Code
                } else if opens_block(rest) {
                    State::TestItem
                } else {
                    lines.push((index + 1, line));
                    State::Code
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
    // A test-only item whose header does not end in `{` has no column-0 closer, so the
    // scan reads on: a `const` broken after `=`, a byte-string fixture, a same-line one.
    let broken_const = "#[cfg(test)]\nconst X: &str =\n    \"s\";\nuse agent_bridle::Gate;\n";
    assert_eq!(naming_bridle(broken_const), [4]);
    let string_fixture = "#[cfg(test)]\nconst Y: &[u8] = b\"a\nb\";\nuse agent_bridle::Gate;\n";
    assert_eq!(naming_bridle(string_fixture), [4]);
    let same_line = "#[cfg(test)] static Z: &str =\n    \"s\";\nuse agent_bridle::Gate;\n";
    assert_eq!(naming_bridle(same_line), [3]);
    // A `{` that opens a string fixture is not a block: the scan reads on.
    let json_fixture = "#[cfg(test)]\nconst J: &str = r#\"{\n    \"a\": 1\n    }\"#;\n\
                        use agent_bridle::Gate;\nimpl X {\n    fn f(_: agent_bridle::G) {}\n}\n\
                        use agent_bridle::After;\n";
    assert_eq!(naming_bridle(json_fixture), [5, 7, 9]);
    let comment_in_string =
        "#[cfg(test)]\nconst S: &str = \"fn f() { // x }\";\nuse agent_bridle::Gate;\n";
    assert_eq!(naming_bridle(comment_in_string), [3]);
    assert!(opens_string("const J: &str = r#\"{") && !opens_string("let s = \"a\\\"b\";"));
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
