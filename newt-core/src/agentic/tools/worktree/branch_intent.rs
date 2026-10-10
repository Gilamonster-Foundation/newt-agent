//! Refusal classification only; this module never authorizes a native write.
use super::super::{invocation, literal, resembles_git};

#[derive(Debug, PartialEq)]
pub(super) enum Intent {
    Create(String),
    Ambiguous(Option<String>),
}

fn render(words: &[String]) -> String {
    words
        .iter()
        .map(|word| crate::mcp::shell_quote_arg(word))
        .collect::<Vec<_>>()
        .join(" ")
}

fn words(
    program: Option<&str>,
    argv: &[String],
    reliable_cwd: bool,
    known: &dyn Fn(&str, bool) -> bool,
) -> Option<Intent> {
    if !program.is_some_and(resembles_git) {
        return None;
    }
    let literal_argv = argv.iter().all(|word| literal(word).is_some());
    let argv: Vec<_> = argv
        .iter()
        .map(|word| literal(word).unwrap_or_else(|| word.clone()))
        .collect();
    let (verb, args, same_repo) = match invocation(&argv) {
        Ok(parts) => parts,
        Err(_) => {
            let i = argv
                .iter()
                .position(|word| matches!(word.as_str(), "checkout" | "switch" | "branch"))?;
            (argv[i].as_str(), &argv[i + 1..], false)
        }
    };
    if !matches!(verb, "checkout" | "switch" | "branch") {
        return None;
    }
    if !literal_argv {
        return Some(Intent::Ambiguous(None));
    }
    let retry = render(&argv);
    let ambiguous = || Some(Intent::Ambiguous(Some(retry.clone())));
    let create = |name: &str| {
        Some(Intent::Create(format!(
            "git checkout -b {}",
            crate::mcp::shell_quote_arg(name)
        )))
    };
    if verb == "branch" {
        // Positive query vocabulary. Unknown/action flags cannot be hidden by
        // a query flag: branch --list -D is not admitted by this classifier.
        let mut query = args.is_empty();
        let mut operands = Vec::new();
        let mut creation_options = false;
        let mut preserve_creation = false;
        for arg in args {
            match arg.as_str() {
                "--show-current" | "--list" | "-l" | "-a" | "--all" | "-r" | "--remotes"
                | "--contains" | "--no-contains" | "--merged" | "--no-merged" | "--points-at" => {
                    query = true;
                }
                "--no-color" | "--no-column" | "-q" | "--quiet" | "-v" | "-vv" | "--verbose" => {}
                "--no-track" | "--track" | "-t" | "--create-reflog" => creation_options = true,
                "-c" | "-C" | "--copy" | "-f" | "--force" => {
                    creation_options = true;
                    preserve_creation = true;
                }
                value
                    if value.starts_with('-')
                        && value.len() > 1
                        && value[1..].chars().all(|c| matches!(c, 'a' | 'r' | 'v')) =>
                {
                    query |= value.chars().any(|c| matches!(c, 'a' | 'r'));
                }
                value
                    if [
                        "--contains=",
                        "--no-contains=",
                        "--merged=",
                        "--no-merged=",
                        "--points-at=",
                    ]
                    .iter()
                    .any(|prefix| value.starts_with(prefix)) =>
                {
                    query = true;
                }
                value if value.starts_with("--color=") || value.starts_with("--column=") => {}
                value if value.starts_with("--track=") => creation_options = true,
                value if !value.starts_with('-') => operands.push(value),
                _ => return ambiguous(),
            }
        }
        if query && !creation_options {
            return None;
        }
        return match operands.as_slice() {
            [name] | [name, _] if !query => {
                if preserve_creation || operands.get(1).is_some_and(|start| *start != "HEAD") {
                    Some(Intent::Create(retry))
                } else {
                    create(name)
                }
            }
            [] if !creation_options => None,
            _ => ambiguous(),
        };
    }
    let flags: &[&str] = if verb == "checkout" {
        &["-b", "-B", "--orphan"]
    } else {
        &["-c", "-C", "--create", "--force-create", "--orphan"]
    };
    for (i, arg) in args
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .enumerate()
    {
        for flag in flags {
            let name = if arg == flag {
                args.get(i + 1).map(String::as_str)
            } else if flag.starts_with("--") {
                arg.strip_prefix(&format!("{flag}="))
            } else {
                arg.strip_prefix(flag).filter(|name| !name.is_empty())
            };
            if arg == flag || name.is_some() {
                // Reset/orphan operations must not be rewritten into ordinary
                // new-at-HEAD creation advice: preserve their actual intent.
                if matches!(*flag, "-B" | "-C" | "--force-create" | "--orphan") {
                    return Some(Intent::Create(retry));
                }
                let after_name = if arg == flag { i + 2 } else { i + 1 };
                if args[..i]
                    .iter()
                    .any(|option| !matches!(option.as_str(), "-q" | "--quiet"))
                    || args.get(after_name).is_some_and(|start| start != "HEAD")
                {
                    return Some(Intent::Create(retry));
                }
                return name
                    .filter(|name| !name.starts_with('-'))
                    .map_or_else(ambiguous, create);
            }
        }
    }
    let mut detached = false;
    let mut no_guess = false;
    let mut operands = Vec::new();
    for arg in args {
        match arg.as_str() {
            "--" if verb == "checkout" => return None,
            "--detach" | "-d" => detached = true,
            "--no-guess" => no_guess = true,
            "-q" | "--quiet" | "-f" | "--force" | "--discard-changes" => {}
            value if !value.starts_with('-') => operands.push(value),
            _ => return ambiguous(),
        }
    }
    if detached || no_guess {
        return None;
    }
    match operands.as_slice() {
        [] | ["HEAD"] | ["@"] if verb == "checkout" => None,
        [_, _, ..] if verb == "checkout" => None,
        [name] if reliable_cwd && same_repo && known(name, verb == "checkout") => None,
        _ => ambiguous(),
    }
}

pub(super) fn classify(source: &str, known: &dyn Fn(&str, bool) -> bool) -> Option<Intent> {
    fn inspect(
        inspection: &agent_bridle::ShellInspection,
        known: &dyn Fn(&str, bool) -> bool,
    ) -> Option<Intent> {
        let stable_cwd = inspection.constructs.is_empty()
            && !inspection
                .commands
                .iter()
                .any(|c| matches!(c.program.as_deref(), Some("cd" | "pushd" | "popd")));
        inspection
            .commands
            .iter()
            .find_map(|command| {
                let direct = command
                    .argv
                    .first()
                    .is_some_and(|word| command.source.trim_start().starts_with(word));
                words(
                    command.program.as_deref(),
                    &command.argv,
                    stable_cwd && direct,
                    known,
                )
                .or_else(|| {
                    command
                        .descendant_execs
                        .iter()
                        .find_map(|child| words(Some(&child.program), &child.argv, false, known))
                })
            })
            .or_else(|| {
                inspection.constructs.iter().find_map(|construct| {
                    construct
                        .inspection
                        .as_deref()
                        .and_then(|nested| inspect(nested, &|_, _| false))
                })
            })
    }
    match agent_bridle::inspect_shell(source) {
        Ok(inspection) => inspect(&inspection, known),
        Err(_) => {
            // Opaque dispatch cannot establish creation intent. It may force
            // refusal, but must not turn a query/switch into creation advice.
            static MARKER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
                regex::Regex::new(r"(?is)\bgit(?:\.exe)?\b.*\b(?:checkout|switch|branch)\b")
                    .expect("fixed branch marker")
            });
            let text: String = source
                .replace("\\\n", "")
                .chars()
                .filter(|c| !matches!(c, '\'' | '"' | '\\'))
                .collect();
            MARKER.is_match(&text).then_some(Intent::Ambiguous(None))
        }
    }
}

#[cfg(test)]
#[path = "branch_intent_tests.rs"]
mod tests;
