use super::{cd_command, confine_under_root, lexical_normalize};
use std::path::{Path, PathBuf};

#[test]
fn cd_command_parses_only_slash_cd() {
    // Regression (#1096): `/cd` is the ONLY human navigation command; the
    // bare `cd`/`pwd`/`ls`/`rm`/… verbs were retired, so bare text is never
    // intercepted (it goes to the model, like Claude Code).
    assert_eq!(cd_command("/cd"), Some(""));
    assert_eq!(cd_command("/cd src"), Some("src"));
    assert_eq!(cd_command("  /cd  src  "), Some("src"));
    assert_eq!(cd_command("/cd ../.."), Some("../.."));
    // NOT a `/cd`: the retired bare verb, a longer word, another command.
    assert_eq!(cd_command("cd src"), None);
    assert_eq!(cd_command("pwd"), None);
    assert_eq!(cd_command("/cdr"), None);
    assert_eq!(cd_command("/cdate"), None);
    assert_eq!(cd_command("hello /cd"), None);
}

/// **The one behaviour the counted-interception rewrite changed** (#2009 PR2).
///
/// `strip_prefix("/cd")` accepted exactly one leading slash, so `/cd` was the
/// single verb in the shell that refused a doubled slash — every other command
/// reaches its handler through `trim_start_matches('/')` and has always
/// accepted `//help`. This is `/cd` joining them, pinned here so the change is
/// a recorded decision rather than something discovered later by an operator
/// with a sticky key.
#[test]
fn a_doubled_slash_reaches_cd_the_way_it_reaches_every_other_verb() {
    assert_eq!(cd_command("//cd"), Some(""));
    assert_eq!(cd_command("//cd src"), Some("src"));
    // The near-miss class is still refused, and now by construction: `/cdate`
    // parses to the verb `cdate`, which is not `cd`.
    assert_eq!(cd_command("//cdate"), None);
    assert_eq!(cd_command("//cd/src"), None, "not a whitespace-split arg");
}

#[test]
fn confine_keeps_cd_under_the_root() {
    let root = Path::new("/w");
    let cwd = PathBuf::from("/w/a");
    // A descent stays confined and normalizes.
    assert_eq!(
        confine_under_root(root, &cwd, "b"),
        Some(PathBuf::from("/w/a/b"))
    );
    // Climbing back to the root is allowed.
    assert_eq!(
        confine_under_root(root, &cwd, ".."),
        Some(PathBuf::from("/w"))
    );
    // Climbing ABOVE the root, or an absolute escape, is refused.
    assert_eq!(confine_under_root(root, &cwd, "../.."), None);
    assert_eq!(confine_under_root(root, &cwd, "/etc"), None);
}

#[test]
fn lexical_normalize_collapses_dot_segments() {
    assert_eq!(
        lexical_normalize(Path::new("/w/a/../b")),
        PathBuf::from("/w/b")
    );
}
