//! #3160: `shell_inspect` is a copy of agent-bridle-tool-shell's inspector, so newt-core
//! needs no bridle crate to read shell syntax. While bridle is a dependency the two must
//! agree on every input: the same inspection (compared as JSON), the same refusal, the same
//! literal word. The corpus is every string literal in the inherited tests, plus `extras`:
//! the boundary inputs and the inputs those tests build at run time.

use crate::shell_inspect as ours;

const CORPUS: &[&str] = &[
    r#"ls -1 $(find . -name "*.rs" -type f -exec wc -l {} + 2>/dev/null | sort -nr | head -10)"#,
    r#"$(find . -name "*.rs" -type f -exec wc -l {} + 2>/dev/null | sort -nr | head -10)"#,
    r#"find . -name "*.rs" -type f -exec wc -l {} + 2>/dev/null"#,
    r#"echo "$(printf '%s' "$(uname)")" $(pwd)"#,
    r#"for f in $(find . -name '*.rs' -type f); do wc -l "$f"; done"#,
    r#"wc -l "$f""#,
    r#"for f in $(find . -name '*.rs' -type f -not -path '*/target/*' -not -path '*/.git/*'); do wc -l "$f"; done | sort -nr | head -10"#,
    r#"find . -name '*.rs' -type f -not -path '*/target/*' -not -path '*/.git/*'"#,
    r#"echo "$((1 + 2))" `pwd`"#,
    r#"echo "$((1 + 2 * (3 - 4)))" "$((1 ? 2 : 3))" "$((16#ff & 7))" "$[4 + 5]""#,
    r#"payload='array[$(printf SHOULD_NOT_RUN >&2)0]'; echo "$((payload))""#,
    r#"index='$(printf SHOULD_NOT_RUN >&2)'; echo "$((array[index]))""#,
    r#"echo "${array[1 + 2]}" "${value:1 + 2:3}""#,
    r#"echo "${array[index]}""#,
    r#"echo "${value:offset:2}""#,
    r#"echo "${value:1:length}""#,
    r#"payload='$(printf SHOULD_NOT_RUN >&2)'; echo "${payload@P}""#,
    r#"ref='array[$(printf SHOULD_NOT_RUN >&2)0]'; echo "${!ref}""#,
    r#"printf -- λπ"$(echo café)""#,
    r#"eval "$(generate-script)""#,
    r#"source "$(choose-file)""#,
    r#"builtin eval "$(generate-script)""#,
    r#"command source "$(choose-file)""#,
    r#"command find . -exec rm {} +"#,
    r#"env MODE=test sh -c "run-hidden-command""#,
    r#"xargs rm"#,
    r#"/usr/bin/env python3 -c "run_hidden_command()""#,
    r#"/opt/tools/python3.12 -c "run_hidden_command()""#,
    r#"trap 'run-hidden-command' EXIT"#,
    r#"printf -v result '%s' value"#,
    r#"printf -vresult '%s' value"#,
    r#"printf '-v' result '%s' value"#,
    r#"option=-v; printf "$option" result '%s' value"#,
    r#"printf -- -v literal"#,
    r#"printf '%s' -v literal"#,
    r#"C:/Tools/PYTHON.EXE -c "run_hidden_command()""#,
    r#"C:/Tools/PowerShell.ExE -Command "run-hidden-command""#,
    r#"C:/Tools/PwSh.ExE -Command "run-hidden-command""#,
    r#"C:/Windows/System32/CmD.ExE /C "run-hidden-command""#,
    r#"C:/Windows/System32/FIND.EXE . -exec WC.EXE -l {} +"#,
    r#"find . -exec printf '[%s]\n' + {} \;"#,
    "timeout 120",
    "'timeout' --foreground --preserve-status --verbose -s TERM -k 2s 0.5m",
    "/usr/bin/timeout --signal=9 --kill-after=1s -- 3h",
    "printf before; {prefix} 'cargo' check --offline 2>&1 | tail -30",
    "cargo",
    "'cargo'",
    "check",
    "--offline",
    "'cargo' check --offline",
    "cannot independently intercept",
    "timeout --unknown 2 cargo check",
    "timeout --help 2 cargo check",
    "timeout --version 2 cargo check",
    "timeout -s \"$SIGNAL\" 2 cargo check",
    "timeout $SECONDS cargo check",
    "timeout 2 \"$PROGRAM\" check",
    "timeout 2 sh -c 'cargo check'",
    "timeout 2 timeout 3 cargo check",
    "timeout NaN cargo check",
    "timeout -1 cargo check",
    "timeout 2",
    "{source}",
    "timeout 2 find . -exec git commit -m message \\;",
    "find",
    "git",
    "timeout 2 cargo check --package \"$(printf package)\"",
    "find . -exec timeout 2 git commit -m delegated \\;",
    "timeout 3 find . -exec timeout 2 git commit -m delegated \\;",
    "git commit -m direct; find . -exec timeout 2 git commit -m delegated \\;",
    "literal $(touch never) `touch neither` 'unmatched\n",
    "cat <<'EOF'\n{body}EOF\n",
    "quoted heredoc inspection",
    "cat",
    "cat <<EOF\n'$(printf nested)' `printf backquoted`\nEOF\n",
    "expanding heredoc inspection",
    "printf",
    "git branch --show-current && git commit -m \"$(cat <<'EOF'\nsubject\n\nbody\nEOF\n)\" 2>&1 | tail -5",
    "compound heredoc inspection",
    "tail",
    "subject\n\nbody\n",
    "cat <<EOF\n$(if)\nEOF\n",
    "cat <<EOF\n$(printf -v target value)\nEOF\n",
    "cat <<EOF\n${value@P}\nEOF\n",
    "must retain nested refusal: {source}",
    "inspection",
    "ls",
    "-1",
    "inner inspection",
    "sort -nr",
    "head -10",
    "/dev/null",
    "wc",
    "-l",
    "{}",
    "-exec wc -l {} +",
    "find -exec",
    "first inner",
    "$(uname)",
    "find substitution",
    "flattened control-flow projection",
    ".",
    "-name",
    "'*.rs'",
    "-type",
    "f",
    "-not",
    "-path",
    "'*/target/*'",
    "'*/.git/*'",
    "sort",
    "head",
    "static arithmetic inspection",
    "1 + 2 * (3 - 4)",
    "1 ? 2 : 3",
    "16#ff & 7",
    "4 + 5",
    "echo $((counter))",
    "echo $[counter]",
    "echo $((array[index]))",
    "echo $((counter += 1))",
    "echo $((++counter))",
    "counter=2; echo $((counter + 1))",
    "runtime-state arithmetic must fail closed",
    "runtime shell state",
    "unexpected refusal for `{source}`: {error}",
    "echo $(( $counter + 1 ))",
    "echo $(( ${counter} + 1 ))",
    "echo $(( $(printf 1) + 1 ))",
    "echo $(( `printf 1` + 1 ))",
    "echo $((1 + $((2))))",
    "nested expansion must fail closed: {source}",
    "state-free parameter arithmetic",
    "parameter array subscript depends on runtime shell state",
    "parameter substring offset depends on runtime shell state",
    "parameter substring length depends on runtime shell state",
    "prompt-expanding parameter transformation",
    "indirect parameter expansion",
    "runtime-derived parameter evaluation must fail closed",
    "$(echo café)",
    "echo café",
    "echo $(uname",
    "$PROGRAM --version",
    "find . -exec \"$PROGRAM\" {} +",
    "a dispatcher or interpreter must not bypass inspection",
    "cannot be projected statically",
    "fc -s",
    "a deferred evaluator must not be projected",
    "Brush printf -v must not reparse a target",
    "`printf -v`",
    "reparses its assignment target",
    "a dynamic first argument could select -v",
    "dynamic first argument",
    "could select `-v`",
    "unexpected dynamic-option refusal: {dynamic}",
    "`--` makes -v the literal format operand",
    "arguments after the format operand cannot select -v",
    "python.exe",
    "python",
    "PowerShell.EXE",
    "powershell",
    "pwsh.exe",
    "pwsh",
    "find.exe",
    "cmd.exe",
    "cmd",
    "python.COM",
    "python.BAT",
    "python.CMD",
    "unexpected classification for {program}",
    "a Windows-spelled interpreter must fail closed",
    "Windows-spelled find inspection",
    "C:/Windows/System32/FIND.EXE",
    "WC.EXE",
    "-exec WC.EXE -l {} +",
    "printf a | head -1 && printf b &",
    "flattened inspection",
    "pipeline topology",
    "conditional relationship",
    "background lifecycle",
    "[%s]\\n",
    "+",
    "a literal plus argument must not truncate the disclosed child argv",
    "agent-bridle-shell-inspect-no-exec-{}-{suffix}",
    "echo $(touch '{}')",
    "inspection executed the substitution",
    "printf ok",
    "serialize",
    "source",
    "schema_version",
    "commands",
    "program",
    "argv",
    "redirects",
    "descendant_execs",
    "constructs",
    "warnings",
];

const WORDS: &[(&str, Option<&str>)] = &[
    ("@(a|b)", None),
    ("+(x)", None),
    ("!(x)", None),
    ("?(x)", None),
    ("*(x)", None),
    ("a@(b)", None),
    ("!()", None),
    ("{a}{b,c}", None),
    ("{}{1..2}", None),
    ("'{a}'{b,c}", None),
    ("x{a}{,}", None),
    ("{a}{b,c}{d}", None),
    ("a=(b c)", None),
    ("*", None),
    ("x.*", None),
    ("[", None),
    ("{a,b}", None),
    ("{1..2}", None),
    ("literal", Some("literal")),
    ("owner/r", Some("owner/r")),
    ("done", Some("done")),
    ("a=~/x", Some("a=~/x")),
    ("{a}b{c}", Some("{a}b{c}")),
    ("''", Some("")),
    ("'two words'", Some("two words")),
    ("'!(x)'", Some("!(x)")),
    (r#""@(a|b)""#, Some("@(a|b)")),
    (r"a\(b", Some("a(b")),
    (r"@\(a\|b\)", Some("@(a|b)")),
    ("'a=(b c)'", Some("a=(b c)")),
    (r"a=\(b\ c\)", Some("a=(b c)")),
    ("'*.rs'", Some("*.rs")),
    ("résumé", Some("résumé")),
];

fn extras() -> Vec<String> {
    let mut inputs: Vec<String> = ["", " ", "\n", "\0", "é ü $(echo é)"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    // The size limit exactly, and one byte over it.
    inputs.push("a".repeat(ours::MAX_SOURCE_BYTES));
    inputs.push("a".repeat(ours::MAX_SOURCE_BYTES + 1));
    // Nesting deeper than the depth limit.
    inputs.push(format!("{}x{}", "$(".repeat(40), ")".repeat(40)));
    for prefix in [
        "timeout 120",
        "'timeout' --foreground --preserve-status --verbose -s TERM -k 2s 0.5m",
        "/usr/bin/timeout --signal=9 --kill-after=1s -- 3h",
    ] {
        inputs.push(format!(
            "printf before; {prefix} 'cargo' check --offline 2>&1 | tail -30"
        ));
    }
    // The inherited tests build two more inputs at run time, so the literals in `CORPUS`
    // are their parts and not what the inspector is given: a quoted heredoc whose body
    // holds a substitution, a backquote and an unmatched quote, and a substitution around
    // a quoted path.
    let body = "literal $(touch never) `touch neither` 'unmatched\n";
    inputs.push(format!("cat <<'EOF'\n{body}EOF\n"));
    inputs.push("echo $(touch '/tmp/newt-inspect-marker')".to_string());
    for (word, _) in WORDS {
        inputs.extend([
            (*word).to_string(),
            format!("printf {word} literal"),
            format!("find . -exec {word} \\;"),
            format!("find . -exec word-probe {word} \\;"),
        ]);
    }
    inputs
}

#[test]
fn the_copy_inspects_every_input_as_bridle_does() {
    let inputs: Vec<String> = CORPUS
        .iter()
        .map(|s| (*s).to_string())
        .chain(extras())
        .collect();
    let (mut accepted, mut refused) = (0, 0);
    for input in &inputs {
        let (a, b) = (
            ours::inspect_shell(input),
            agent_bridle::inspect_shell(input),
        );
        match (&a, &b) {
            (Ok(a), Ok(b)) => {
                assert_eq!(
                    serde_json::to_value(a).unwrap(),
                    serde_json::to_value(b).unwrap(),
                    "{input:?}"
                );
                accepted += 1;
            }
            (Err(a), Err(b)) => {
                assert_eq!(format!("{a:?}"), format!("{b:?}"), "{input:?}");
                refused += 1;
            }
            _ => {
                panic!("one inspector accepted {input:?} and the other refused it: {a:?} vs {b:?}")
            }
        }
        assert_eq!(
            ours::literal_shell_word(input).map_err(|e| format!("{e:?}")),
            agent_bridle_tool_shell::literal_shell_word(input).map_err(|e| format!("{e:?}")),
            "{input:?}"
        );
    }
    // Anti-vacuous: the corpus reaches both outcomes in numbers.
    assert!(
        accepted > 50 && refused > 20,
        "accepted {accepted}, refused {refused} of {}",
        inputs.len()
    );
}

#[test]
fn static_word_corpus_has_explicit_runtime_expectations() {
    for &(word, expected) in WORDS {
        assert_eq!(
            ours::literal_shell_word(word).ok().as_deref(),
            expected,
            "{word}"
        );
        assert_eq!(
            agent_bridle_tool_shell::literal_shell_word(word)
                .ok()
                .as_deref(),
            expected,
            "{word}"
        );
        let find = format!("find . -exec word-probe {word} \\;");
        match expected {
            Some(value) => {
                assert_eq!(
                    ours::inspect_shell(&find).unwrap().commands[0].descendant_execs[0].argv[1],
                    value
                );
                assert!(
                    ours::inspect_shell(&format!("printf {word} literal")).is_ok(),
                    "{word}"
                );
            }
            None => {
                assert!(ours::inspect_shell(&find).is_err(), "{word}");
                assert!(
                    ours::inspect_shell(&format!("printf {word} literal")).is_err(),
                    "{word}"
                );
                assert!(
                    ours::inspect_shell(&format!("find . -exec {word} \\;")).is_err(),
                    "{word}"
                );
                assert!(
                    ours::inspect_shell(word).map_or(true, |inspection| inspection
                        .commands
                        .iter()
                        .all(|command| command.program.is_none())),
                    "{word}"
                );
            }
        }
    }
}
