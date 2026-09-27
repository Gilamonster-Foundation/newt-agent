//! #2274 parent-routing regressions. Syntax lowering must preserve the original
//! Brush AST and must never turn a noncandidate into named-root authority.

use super::{lower_named_host_root_command, select_named_host_root};
use agent_bridle_core::{Caveats, Scope};

const ROOT: &str = if cfg!(windows) {
    r"C:\tools\cargo.exe"
} else {
    "/tools/cargo"
};

#[test]
fn one_absolute_root_retains_literal_argv() {
    let command = format!("'{ROOT}' build --offline 'literal;argument'");
    let (program, argv) = lower_named_host_root_command(&command)
        .expect("valid Brush syntax")
        .expect("one absolute literal root");
    assert_eq!(program, ROOT);
    assert_eq!(argv, ["build", "--offline", "literal;argument"]);
}

#[test]
fn compound_or_dynamic_source_never_selects_the_root_operation() {
    for command in [
        format!("'{ROOT}' build\n'/tools/other'"),
        format!("'{ROOT}' build && echo later"),
        format!("'{ROOT}' build | cat"),
        format!("'{ROOT}' build &"),
        format!("! '{ROOT}' build"),
        format!("time '{ROOT}' build"),
        format!("MODE=one '{ROOT}' build"),
        format!("'{ROOT}' build > output"),
        format!("'{ROOT}' build $(echo extra)"),
        format!("'{ROOT}' build $FLAGS"),
        format!("'{ROOT}' build ~/project"),
        format!("'{ROOT}' build {{one,two}}"),
        format!("'{ROOT}' build *.rs"),
        format!("export PATH; '{ROOT}' build"),
        format!("if true; then '{ROOT}' build; fi"),
        format!("for item in one; do '{ROOT}' build; done"),
        format!("( '{ROOT}' build )"),
        format!("{{ '{ROOT}' build; }}"),
        format!("run_it() {{ '{ROOT}' build; }}; run_it"),
        format!("'{ROOT}' build < <(echo source)"),
        format!("'{ROOT}' build <<< source"),
        format!("'{ROOT}' build $'escaped'"),
        format!("'{ROOT}' build $((1 + 1))"),
    ] {
        assert!(
            lower_named_host_root_command(&command)
                .expect("valid noncandidate syntax must return Ok(None), not refuse")
                .is_none(),
            "must preserve the ordinary Brush route: {command}"
        );
    }
}

#[test]
fn bare_program_names_do_not_select_named_root() {
    for command in ["cargo build", "env cargo build", "sh -c 'cargo build'"] {
        assert!(lower_named_host_root_command(command)
            .expect("a bare program remains valid ordinary Brush input")
            .is_none());
    }
}

#[test]
fn an_absolute_interpreter_is_not_excluded_by_a_basename_blacklist() {
    let interpreter = if cfg!(windows) {
        r"C:\tools\shell.exe"
    } else {
        "/bin/sh"
    };
    let command = format!("'{interpreter}' -c 'echo literal'");
    let (program, argv) = lower_named_host_root_command(&command)
        .expect("valid literal input")
        .expect("an absolute interpreter is a candidate root like any other program");
    assert_eq!(program, interpreter);
    assert_eq!(argv, ["-c", "echo literal"]);
    // Actual execution still requires the exact grant and admitted fs/net fence.
}

#[test]
fn malformed_syntax_can_return_the_normal_parse_error() {
    assert!(lower_named_host_root_command(&format!("'{ROOT}' 'unterminated")).is_err());
}

#[test]
fn quoted_literals_preserve_empty_words_unicode_and_metacharacters() {
    let command = format!(r#"'{ROOT}' '' 'résumé' "literal * ${{not_expanded}}" '# ignored?'"#);
    // The double-quoted parameter is still dynamic; quoting cannot turn it
    // into a literal argument. A single-quoted spelling can.
    assert!(lower_named_host_root_command(&command).unwrap().is_none());
    let command = format!(r#"'{ROOT}' '' 'résumé' 'literal * ${{not_expanded}}' '# ignored?'"#);
    let (_, argv) = lower_named_host_root_command(&command).unwrap().unwrap();
    assert_eq!(
        argv,
        ["", "résumé", "literal * ${not_expanded}", "# ignored?"]
    );
}

#[test]
fn comments_do_not_become_arguments_or_hide_a_second_command() {
    let command = format!("'{ROOT}' build # ordinary comment");
    let (_, argv) = lower_named_host_root_command(&command).unwrap().unwrap();
    assert_eq!(argv, ["build"]);
    let command = format!("'{ROOT}' build # comment\necho second");
    assert!(lower_named_host_root_command(&command).unwrap().is_none());
}

#[test]
fn parent_traversal_never_selects_a_different_root_spelling() {
    let root = if cfg!(windows) {
        r"C:\tools\link\..\cargo.exe"
    } else {
        "/tools/link/../cargo"
    };
    assert!(lower_named_host_root_command(&format!("'{root}' build"))
        .unwrap()
        .is_none());
}

#[test]
fn selection_requires_opt_in_and_an_exact_finite_grant() {
    let source = format!("'{ROOT}' build");
    let exact = Caveats {
        exec: Scope::only([ROOT.to_string()]),
        ..Caveats::top()
    };
    assert!(select_named_host_root(true, &source, &exact)
        .unwrap()
        .is_some());
    assert!(select_named_host_root(false, &source, &exact)
        .unwrap()
        .is_none());
    for exec in [
        Scope::All,
        Scope::none(),
        Scope::only(["cargo".to_string()]),
    ] {
        let caveats = Caveats {
            exec,
            ..Caveats::top()
        };
        assert!(select_named_host_root(true, &source, &caveats)
            .unwrap()
            .is_none());
    }
    let sibling = if cfg!(windows) {
        r"C:\other\cargo.exe"
    } else {
        "/other/cargo"
    };
    assert!(
        select_named_host_root(true, &format!("'{sibling}' build"), &exact)
            .unwrap()
            .is_none()
    );
}

#[test]
fn disabled_route_does_not_parse_or_change_ordinary_errors() {
    assert!(
        select_named_host_root(false, "'unterminated", &Caveats::top())
            .unwrap()
            .is_none()
    );
}

/// #2274: opt-in routing must not add unbounded synchronous parent parsing.
#[test]
fn named_host_root_source_limit_preserves_the_ordinary_route() {
    let prefix = format!("'{ROOT}' '");
    let padding = crate::shell_inspect::MAX_SOURCE_BYTES - prefix.len() - 1;
    let boundary = format!("{prefix}{}'", "x".repeat(padding));
    assert_eq!(boundary.len(), crate::shell_inspect::MAX_SOURCE_BYTES);
    assert!(lower_named_host_root_command(&boundary).unwrap().is_some());
    assert!(lower_named_host_root_command(&(boundary + " "))
        .unwrap()
        .is_none());
}

// Model: gpt-6-astra | Harness: Codex 0.153.4 | Operator: Shawn Hartsock | Time: 22:29 UTC | Date: 2026-09-12
