//! Pin admitted Git and display words using staging's executable trust (#2733, #2810).
use crate::{git_staging, Caveats};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub(super) fn authenticate(path: &OsStr, caveats: &Caveats) -> Result<PathBuf, ()> {
    authenticate_program(path, caveats, "git")
}

fn authenticate_program(path: &OsStr, caveats: &Caveats, name: &str) -> Result<PathBuf, ()> {
    // The shared lookup skips relative PATH entries; a shell would not. Never
    // infer that its absolute result is what an unresolved shell name means.
    if std::env::split_paths(path).any(|dir| !dir.is_absolute()) {
        return Err(());
    }
    // Same resolver used by staging and #2743 diagnostics, via its Git wrapper.
    let program = crate::git_hardening::resolve_trusted_program(Path::new("."), Some(path), name)
        .map_err(|_| ())?;
    let resolved = program.canonicalize().map_err(|_| ())?;
    let mut trust = git_staging::TrustContext::bind(&caveats.fs_write).map_err(|_| ())?;
    if name == "git" {
        trust = trust.with_git(&resolved);
    }
    git_staging::trust_check(&program, &trust).map_err(|_| ())?;
    Ok(resolved)
}

pub(super) fn resolve(caveats: &Caveats) -> Result<PathBuf, ()> {
    resolve_program(caveats, "git")
}

fn resolve_program(caveats: &Caveats, name: &str) -> Result<PathBuf, ()> {
    // Use the same venv/exec-path/developer-tool selection as shell dispatch.
    // In particular macOS may select the real tool instead of /usr/bin's shim.
    let env = super::shell::venv_env_map();
    let path = env
        .get("PATH")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
        .ok_or(())?;
    if name == "git" {
        authenticate(&path, caveats)
    } else {
        authenticate_program(&path, caveats, name)
    }
}

/// For an already-admitted batch, replace only inspected executable words.
/// Restrict gaps to exact shell
/// connectors/fd duplication: comments, assignments, loops, background jobs,
/// or ambiguous source projection refuse rather than guessing token offsets.
/// Keep arguments, cwd selectors, redirects and short-circuit operators intact.
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
            pinned.push_str(&quoted);
            pinned.push_str(&command.source[word.len()..]);
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
