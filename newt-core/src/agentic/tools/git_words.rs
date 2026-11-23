//! What a shell source says about `git` and `gh`: which program a word names, which verb
//! an argv runs, and the loose triggers that send a `run_command` source to a governed
//! broker. The dispatcher, the publication route and the worktree guard read these as well
//! as the brokers in `native_git`, so they live outside it (#3160).
use crate::shell_inspect::{inspect_shell, ShellInspection};

/// Every `git` invocation `source` makes, as `(verb, arguments)`, read from its bare
/// words without the shell inspector: segments split at shell punctuation, and `git` taken
/// anywhere in a segment, so `then git commit -m x` and `xargs git branch -d` count.
/// A tripwire, not a boundary. Words are split at white space and keep their quotes,
/// so `'git fetch'` or `"git` is not git, but git text inside a longer quoted
/// string, a comment or a here-document is read as an invocation wherever no quote
/// touches the `git` or the verb. An obfuscated command stays the confined
/// executor's.
pub(super) fn bare_git_invocations(source: &str) -> Vec<(String, Vec<String>)> {
    let mut found = Vec::new();
    for segment in source.split(['|', ';', '&', '\n', '(', ')', '`', '<', '>']) {
        let words: Vec<String> = segment.split_whitespace().map(str::to_string).collect();
        for (at, word) in words.iter().enumerate() {
            if is_git(word) {
                if let Ok((verb, args, _)) = invocation(&words[at..]) {
                    found.push((verb.to_string(), args.to_vec()));
                }
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
            .filter_map(|(verb, _)| literal(&verb))
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
/// WHOLE `run_command` call to [`execute_governed_push`] instead of the
/// confined shell — that function applies the source-bound/fixed-argv
/// check and refuses anything this loose scan admits but the broker cannot
/// safely execute. Harmless output wrappers are interpreted in-process (#2719).
///
/// [`execute_governed_push`]: super::native_git::execute_governed_push
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
