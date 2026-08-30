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
