//! Pin admitted Git and display words using staging's executable trust (#2733, #2810).
use crate::Caveats;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub(super) fn authenticate(path: &OsStr, caveats: &Caveats) -> Result<PathBuf, ()> {
    authenticate_program(path, caveats, "git")
}

fn authenticate_program(path: &OsStr, caveats: &Caveats, name: &str) -> Result<PathBuf, ()> {
    crate::exec_grants::trusted_program(path, caveats, name)
}

pub(super) fn resolve(caveats: &Caveats) -> Result<PathBuf, ()> {
    resolve_program(caveats, "git")
}

fn resolve_program(caveats: &Caveats, name: &str) -> Result<PathBuf, ()> {
    // Use the same venv/exec-path/developer-tool selection as shell dispatch.
    // In particular macOS may select the real tool instead of /usr/bin's shim.
    let path = crate::exec_grants::dispatch_path().ok_or(())?;
    if name == "git" {
        authenticate(&path, caveats)
    } else {
        authenticate_program(&path, caveats, name)
    }
}

/// A dot is an operand here, not the shell's source builtin. Bridle refuses
/// inspecting it as a standalone program; admit only these literal spellings.
pub(super) fn argument(word: &str) -> Option<String> {
    if matches!(word, "." | "'.'" | "\".\"") {
        Some(".".into())
    } else {
        super::literal(word)
    }
}

/// Fixed read-only forms; no global options, aliases, paths, or output gadgets.
/// Confined dispatch supplies sandbox_git_env's repository-config hardening;
/// pin() also disables pagers and optional index writes for these siblings.
pub(super) fn read_only(args: &[&str]) -> bool {
    // PR #2827: log can invoke signature verifiers; status can invoke clean
    // filters and submodule summaries (which run log). Neither is read-only
    // under repository config, even with pagers and optional locks disabled.
    matches!(
        args,
        ["branch", "--show-current"]
            | ["worktree", "list"]
            | ["rev-parse", "HEAD" | "--show-toplevel"]
            | ["rev-parse", "--abbrev-ref", "HEAD"]
    )
}

/// Only the explicit start operand of the ordinary -b form treats `.` as HEAD.
/// Never rewrite a cwd, branch name, destination, sibling, or quoted prose.
fn creation_source(
    command: &agent_bridle::InspectedCommand,
    words: &[String],
) -> Result<String, ()> {
    let Some(args) = super::worktree_add_args(words) else {
        return Ok(command.source.clone());
    };
    let args: Vec<_> = args.iter().map(String::as_str).collect();
    if !matches!(args.as_slice(), ["-b", _, _, "."] | [_, "-b", _, "."]) {
        return Ok(command.source.clone());
    }
    // Prove the argv projection is contiguous before using its exact offsets.
    let mut rest = command.source.as_str();
    for word in &command.argv[..command.argv.len() - 1] {
        rest = rest.trim_start().strip_prefix(word).ok_or(())?;
    }
    rest = rest.trim_start();
    let offset = command.source.len() - rest.len();
    let dot = command.argv.last().ok_or(())?;
    let suffix = rest.strip_prefix(dot).ok_or(())?;
    Ok(format!("{}HEAD{suffix}", &command.source[..offset]))
}

/// Pin an admitted batch, harden read-only siblings and normalize a dot start.
/// Restrict gaps to exact shell
/// connectors/fd duplication: comments, assignments, loops, background jobs,
/// or ambiguous source projection refuse rather than guessing token offsets.
/// Otherwise keep arguments, cwd selectors, redirects and shell operators intact.
pub(super) fn pin(source: &str, git: &Path, caveats: &Caveats) -> Result<String, ()> {
    static GAP: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"\A(?:\s|;|&&|\|\||\||[0-9]*[<>]&[0-9]+)*\z")
            .expect("fixed shell gap pattern")
    });
    let inspection = agent_bridle::inspect_shell(source).map_err(|_| ())?;
    if !inspection.constructs.is_empty() {
        return Err(());
    }
    let quoted = crate::mcp::shell_quote_arg(git.to_str().ok_or(())?);
    let mut rest = source;
    let mut pinned = String::new();
    for command in inspection.commands {
        let offset = rest.find(&command.source).ok_or(())?;
        let gap = &rest[..offset];
        let word = command.argv.first().ok_or(())?;
        if !GAP.is_match(gap) || !command.source.starts_with(word) {
            return Err(());
        }
        pinned.push_str(gap);
        if command.program.as_deref() == Some("git") {
            let words = command
                .argv
                .iter()
                .map(|s| argument(s))
                .collect::<Option<Vec<_>>>()
                .ok_or(())?;
            let source = creation_source(&command, &words)?;
            pinned.push_str(&quoted);
            if read_only(&words.iter().skip(1).map(String::as_str).collect::<Vec<_>>()) {
                pinned.push_str(" --no-pager --no-optional-locks");
            }
            pinned.push_str(&source[word.len()..]);
        } else if command.program.as_deref() == Some("tail") {
            let tail = resolve_program(caveats, "tail")?;
            pinned.push_str(&crate::mcp::shell_quote_arg(tail.to_str().ok_or(())?));
            pinned.push_str(&command.source[word.len()..]);
        } else {
            pinned.push_str(&command.source);
        }
        rest = &rest[offset + command.source.len()..];
    }
    if !GAP.is_match(rest) {
        return Err(());
    }
    pinned.push_str(rest);
    Ok(pinned)
}
