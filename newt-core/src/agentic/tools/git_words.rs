//! What a shell source says about `git` and `gh`: which program a word names, which verb
//! an argv runs, and the loose triggers that send a `run_command` source to a governed
//! broker. The dispatcher, the publication route and the worktree guard read these as well
//! as the brokers in `native_git`, so they live outside it (#3160).
use crate::shell_inspect::{inspect_shell, ShellConstructKind, ShellInspection};

/// Every apparent `git` argv, retaining global options for policy validation, read
/// from bare words without the shell inspector: segments split at shell punctuation, and `git` taken
/// anywhere in a segment, so `then git commit -m x` and `xargs git branch -d` count.
/// A tripwire, not a boundary. Words are split at white space and keep their quotes,
/// so `'git fetch'` or `"git` is not git, but git text inside a longer quoted
/// string, a comment or a here-document is read as an invocation wherever no quote
/// touches the `git` or the verb. An obfuscated command stays the confined
/// executor's.
pub(super) fn bare_git_invocations(source: &str) -> Vec<Vec<String>> {
    let mut found = Vec::new();
    for segment in source.split(['|', ';', '&', '\n', '(', ')', '`', '<', '>']) {
        let words: Vec<String> = segment.split_whitespace().map(str::to_string).collect();
        for (at, word) in words.iter().enumerate() {
            if is_git(word) {
                found.push(words[at..].to_vec());
            }
        }
    }
    found
}

/// The first `git` verb in `verbs` that `source` invokes anywhere, nested
/// constructs included. A loose trigger. In a source the inspector can read, text
/// that merely mentions Git is not an invocation. In one it cannot read (#3146) the
/// verb is read from the bare words, see [`bare_git_invocations`], and git text
/// that no quote touches counts even when nothing would run it. A false hit costs:
/// one network prompt the command did not need (a decline refuses the command), or
/// for a push or a commit a refusal of the whole command. Never a missed gate. The
/// push refusal ends with the push retry text, so a command that only mentioned a
/// push is told how to push.
pub(super) fn git_verb_in(source: &str, verbs: &[&str]) -> Option<String> {
    fn find(inspection: &ShellInspection, verbs: &[&str]) -> Option<String> {
        inspection
            .commands
            .iter()
            .find_map(|command| {
                command
                    .program
                    .as_deref()
                    .filter(|program| is_git(program))?;
                let verb = invocation(&command.argv)
                    .ok()
                    .and_then(|(verb, _, _)| literal(verb))?;
                verbs.contains(&verb.as_str()).then_some(verb)
            })
            .or_else(|| {
                inspection
                    .constructs
                    .iter()
                    .find_map(|construct| find(construct.inspection.as_deref()?, verbs))
            })
    }
    match inspect_shell(source) {
        Ok(inspection) => find(&inspection, verbs),
        // #3146: the shell inspector errors on an `if`, `while`, `until`, `case`, subshell
        // or brace group, and one anywhere in a command hid every git verb from the
        // triggers that use this (the network prompt and the commit and push brokers).
        // Read the bare words instead, as the delete tripwire does.
        Err(_) => bare_git_invocations(source)
            .into_iter()
            .filter_map(|argv| {
                invocation(&argv)
                    .ok()
                    .and_then(|(verb, _, _)| literal(verb))
            })
            .find(|verb| verbs.contains(&verb.as_str())),
    }
}

/// Whether `run_command` binds the commit broker to `source`. Only for a source the
/// inspector can read: the broker covers a commit the shell itself runs, not one a
/// dispatcher runs (`find -exec`, `timeout`), and telling the two apart takes the inspection.
pub(super) fn needs_commit_broker(source: &str) -> bool {
    inspect_shell(source).is_ok() && git_verb_in(source, &["commit"]).is_some()
}

/// #3146: whether `source`, which the inspector cannot read, runs `git commit` by its bare
/// words. No broker can be bound to it (see [`needs_commit_broker`]), so `run_command`
/// refuses the whole command before any of it runs. With the broker bound instead, a direct
/// commit landed before the confined shell refused the commit a dispatcher ran beside it.
pub(super) fn commit_in_an_uninspectable_source(source: &str) -> bool {
    inspect_shell(source).is_err() && git_verb_in(source, &["commit"]).is_some()
}

/// #3296: substitutions expand before the native broker sees Git's argv. Inspect
/// every commit argument rather than duplicating Git's message-option grammar
/// (including bundled short flags and attached values). Other commands' arguments
/// and literal code spans are unaffected; nested commits receive the same check.
/// A single `cat` of a quoted here-document only captures literal message data.
pub(super) fn commit_has_command_substitution(source: &str) -> bool {
    fn literal_heredoc_capture(body: &str) -> bool {
        use brush_parser::ast::{
            Command, CommandPrefixOrSuffixItem, CompoundListItem, IoRedirect, SeparatorOperator,
        };
        let options = brush_parser::ParserOptions::default();
        let Ok(program) =
            brush_parser::Parser::new(std::io::Cursor::new(body.as_bytes()), &options)
                .parse_program()
        else {
            return false;
        };
        let [list] = program.complete_commands.as_slice() else {
            return false;
        };
        let [CompoundListItem(list, SeparatorOperator::Sequence)] = list.0.as_slice() else {
            return false;
        };
        let [Command::Simple(command)] = list.first.seq.as_slice() else {
            return false;
        };
        list.additional.is_empty()
            && !list.first.bang
            && list.first.timed.is_none()
            && command.prefix.is_none()
            && command.word_or_name.as_ref().and_then(|word| literal(&word.value)).as_deref()
                == Some("cat")
            // The inspection projection omits delimiter quoting. Read that fact
            // from Brush's AST; never inspect the literal document's contents.
            && matches!(command.suffix.as_ref().map(|suffix| suffix.0.as_slice()),
                Some([CommandPrefixOrSuffixItem::IoRedirect(IoRedirect::HereDocument(None | Some(0), document))])
                    if !document.requires_expansion)
    }
    fn contains(inspection: &ShellInspection) -> bool {
        if inspection.constructs.is_empty() {
            return false;
        }
        inspection.commands.iter().any(|command| {
            command.program.as_deref().is_some_and(is_git)
                && invocation(&command.argv)
                    .is_ok_and(|(verb, _, _)| literal(verb).as_deref() == Some("commit"))
                && command.argv.iter().any(|word| {
                    inspect_shell(&format!(": {word}")).is_ok_and(|word| {
                        word.constructs.iter().any(|construct| {
                            construct.kind == ShellConstructKind::BackquoteSubstitution
                                || (construct.kind == ShellConstructKind::CommandSubstitution
                                    && !literal_heredoc_capture(&construct.body))
                        })
                    })
                })
        }) || inspection
            .constructs
            .iter()
            .any(|construct| construct.inspection.as_deref().is_some_and(contains))
    }
    inspect_shell(source).is_ok_and(|inspection| contains(&inspection))
}

pub(super) fn unresolved(detail: &str) -> String {
    format!("refused: {detail}; native Git ref mutation is not yet supported by the repository authority adapter")
}

pub(super) fn is_gh(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    name.eq_ignore_ascii_case("gh") || name.eq_ignore_ascii_case("gh.exe")
}

/// issue-1188: is `source` a `git push` invocation anywhere in it (a loose
/// trigger; unlike [`needs_commit_broker`] it also fires on the bare words of a
/// source the inspector cannot read)? A `true` here routes the
/// WHOLE `run_command` call to `native_git::execute_governed_push` instead of the
/// confined shell — that function applies the source-bound/fixed-argv
/// check and refuses anything this loose scan admits but the broker cannot
/// safely execute. Harmless output wrappers are interpreted in-process (#2719).
pub(super) fn needs_push_broker(source: &str) -> bool {
    git_verb_in(source, &["push"]).is_some()
}

/// issue-1188 amendment A4: is `source` a `gh pr create` invocation anywhere
/// in it? Same loose-trigger/strict-execute split as [`needs_push_broker`].
pub(super) fn needs_pr_create_broker(source: &str) -> bool {
    fn contains(inspection: &ShellInspection) -> bool {
        inspection.commands.iter().any(|command| {
            command.program.as_deref().is_some_and(is_gh)
                && command.argv.get(1).and_then(|a| literal(a)).as_deref() == Some("pr")
                && command.argv.get(2).and_then(|a| literal(a)).as_deref() == Some("create")
        }) || inspection
            .constructs
            .iter()
            .any(|construct| construct.inspection.as_deref().is_some_and(contains))
    }
    inspect_shell(source).is_ok_and(|inspection| contains(&inspection))
}

pub(super) fn is_git(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    name.eq_ignore_ascii_case("git") || name.eq_ignore_ascii_case("git.exe")
}

/// Resolve a single argument with the shell inspector's word parser. Argument data is not
/// a standalone command: e.g. an owner/repo ending in `/r` is not an interpreter.
pub(super) fn literal(word: &str) -> Option<String> {
    crate::shell_inspect::literal_shell_word(word).ok()
}

/// What to do instead when a delete names what it destroys only at runtime.
pub(super) const NAME_IT_LITERALLY: &str = "Retry with run_command writing the command out \
    literally, for example `git branch -D old-a old-b` or `git worktree remove ../old`; \
    newt then asks the operator.";

/// Resolve policy selectors without dropping unknown words or interpreting
/// quoted global options as verbs. Keep argument source for existing consumers.
/// Commit message data is checked by the broker after expansion; read-command
/// data is not a mutation selector. Every argument inspected by mutation guards
/// must be static, since expansion can change both its value and argv position.
pub(super) fn policy_invocation(argv: &[String]) -> Result<(String, &[String]), String> {
    let resolved: Vec<_> = argv.iter().map(|word| literal(word)).collect();
    let words: Vec<_> = resolved
        .iter()
        .zip(argv)
        .map(|(value, source)| value.as_ref().unwrap_or(source).clone())
        .collect();
    let (verb, args, _) = invocation(&words)?;
    let prefix = argv.len() - args.len();
    let checked = if matches!(
        verb,
        "merge"
            | "rebase"
            | "cherry-pick"
            | "revert"
            | "checkout"
            | "switch"
            | "branch"
            | "stash"
            | "symbolic-ref"
            | "reflog"
            | "worktree"
            | "reset"
            | "clean"
    ) {
        argv.len()
    } else {
        prefix
    };
    if resolved[..checked].iter().any(Option::is_none) {
        return Err(format!("refused: Git policy words must be literal; write out the verb, global options and mutation arguments. {NAME_IT_LITERALLY}"));
    }
    Ok((verb.to_string(), &argv[prefix..]))
}

/// Validate before loose broker selection can classify an unknown selector as
/// ungoverned. Descendants and substitutions share the same policy-word rule.
pub(super) fn check_policy_words(inspection: &ShellInspection) -> Result<(), String> {
    for command in &inspection.commands {
        for (program, argv) in
            std::iter::once((command.program.as_deref(), command.argv.as_slice())).chain(
                command
                    .descendant_execs
                    .iter()
                    .map(|child| (Some(child.program.as_str()), child.argv.as_slice())),
            )
        {
            if program.is_some_and(is_git) {
                policy_invocation(argv)?;
            } else if program.is_some_and(is_gh)
                && argv
                    .iter()
                    .skip(1)
                    .take(2)
                    .any(|word| literal(word).is_none())
            {
                return Err("refused: gh policy words must be literal; write out the command and subcommand".into());
            }
        }
    }
    for nested in inspection
        .constructs
        .iter()
        .filter_map(|construct| construct.inspection.as_deref())
    {
        check_policy_words(nested)?;
    }
    Ok(())
}

/// Find the native verb without translating argv. Repository/config selectors
/// remain usable for reads, but mutation cannot rely on the caller's cwd then.
pub(super) fn invocation(argv: &[String]) -> Result<(&str, &[String], bool), String> {
    let mut index = 1;
    let mut same_repository = true;
    while let Some(word) = argv.get(index) {
        match word.as_str() {
            "--no-pager" | "--paginate" | "--no-optional-locks" => index += 1,
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env" => {
                same_repository = false;
                index += 2;
            }
            option
                if option.starts_with("--git-dir=")
                    || option.starts_with("--work-tree=")
                    || option.starts_with("--namespace=")
                    || option.starts_with("--config-env=")
                    || option.starts_with("--exec-path=")
                    || option == "--bare" =>
            {
                same_repository = false;
                index += 1;
            }
            "--literal-pathspecs"
            | "--glob-pathspecs"
            | "--noglob-pathspecs"
            | "--icase-pathspecs"
            | "--no-replace-objects" => index += 1,
            "--version" | "--help" | "--html-path" | "--man-path" | "--info-path" => {
                return Ok(("help", &[], true));
            }
            verb if !verb.starts_with('-') => {
                return Ok((verb, &argv[index + 1..], same_repository));
            }
            _ => return Err(unresolved("unresolved Git global options")),
        }
    }
    Ok(("help", &[], true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_message_substitution_3296_preserves_literal_text_and_other_commands() {
        for source in [
            r#"git commit -m 'Extract `function` and $(literal)'"#,
            r#"git commit -m "Extract \`function\` and \$(literal)""#,
            r#"git commit --message='Extract `function`'"#,
            "git commit -F - <<'EOF'\nExtract `function` and $(literal)\nEOF\n",
            "git commit -m \"$(cat <<'EOF'\nExtract `function`\nEOF\n)\"",
            r#"echo "$(pwd)" && git commit -m 'Extract `function`'"#,
            r#"git log --format="$(printf literal)""#,
            r#"printf '%s' 'git commit -m "Extract `function`"'"#,
            r#"git commit -m "$MESSAGE""#,
            r#"git commit -m "release $((1 + 2))""#,
        ] {
            assert!(!commit_has_command_substitution(source), "{source}");
        }
        for source in [
            r#"git -C repo commit -m "Extract `function_name`""#,
            r#"git.exe commit -m "Extract `function_name`""#,
            r#"echo "$(git commit -m "Extract `function_name`")""#,
            r#"git commit --author="$(printf author)" -m literal"#,
        ] {
            assert!(commit_has_command_substitution(source), "{source}");
        }
    }

    /// The native Git contract's exact command must also be checked without a
    /// filesystem or subprocess, including on hosts that cannot run Seatbelt.
    #[test]
    fn commit_message_substitution_3296_allows_only_literal_heredoc_capture() {
        for source in [
            "cd newt-core && git branch --show-current && git commit -m \"$(cat <<'EOF'\nRefactor fixture\n\nPreserve multiline message semantics.\nLiteral `backticks` and $(printf literal) remain message text.\nEOF\n)\" 2>&1 | tail -5",
            "git commit -m \"$(cat <<\"DELIM\"\nLiteral `unclosed and $(if)\ngit commit -m \"$(prose)\"\nDELIM\n)\"",
            "git commit -m \"$(cat <<\\DELIM\nLiteral $(prose)\nDELIM\n)\"",
            "git commit -m \"$(cat <<-'EOF'\n\tLiteral `backticks`\n\tEOF\n)\"",
        ] {
            assert!(needs_commit_broker(source), "{source}");
            assert!(!commit_has_command_substitution(source), "{source}");
        }
        for source in [
            "git commit -m \"$(cat <<EOF\nExtract `function_name`\nEOF\n)\"",
            "git commit -m \"$(cat <<EOF\n'$(function_name)'\nEOF\n)\"",
            "git commit -m \"$(cat <<'EOF'\nliteral\nEOF\nprintf extra)\"",
            "git commit -m \"$(cat <<'EOF' | cat\nliteral\nEOF\n)\"",
            "git commit -m \"$(cat extra <<'EOF'\nliteral\nEOF\n)\"",
            "git commit -m \"$(ENV=value cat <<'EOF'\nliteral\nEOF\n)\"",
            "git commit -m \"$(cat <<'EOF' > output\nliteral\nEOF\n)\"",
            "git commit -m \"$(cat <<'EOF'\nliteral\nEOF\n) $(function_name)\"",
            "git commit -m \"$(cat <<'EOF'\nliteral\nEOF\n) `function_name`\"",
        ] {
            assert!(needs_commit_broker(source), "{source}");
            assert!(commit_has_command_substitution(source), "{source}");
        }
    }
}
