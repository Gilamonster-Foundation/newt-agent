use super::*;

#[test]
fn add_then_commit_advances_history() {
    let dir = repo_with_commit();
    std::fs::write(dir.path().join("new.txt"), "data\n").unwrap();
    let eng = GitEngine::open(dir.path()).unwrap();
    let caps = GitCaveats::top();

    let staged = eng.add(&caps, &["new.txt".to_string()]).unwrap();
    assert_eq!(staged, vec!["new.txt".to_string()]);
    assert!(eng
        .status(&caps)
        .unwrap()
        .staged
        .iter()
        .any(|f| f.path == "new.txt"));

    let author = Author {
        name: "Bot".into(),
        email: "bot@newt.dev".into(),
    };
    let c = eng.commit(&caps, "add new file", &author).unwrap();
    assert_eq!(c.summary, "add new file");
    assert_eq!(c.author_name, "Bot");

    let log = eng.log(&caps, 10).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].summary, "add new file");
    assert!(eng.status(&caps).unwrap().clean, "clean after commit");

    // The system `git` agrees grit wrote a real, readable history.
    let out = std::process::Command::new("git")
        .current_dir(dir.path())
        .args(["log", "--oneline"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().count(), 2);
}
#[test]
fn branch_creates_a_ref_at_head() {
    let dir = repo_with_commit();
    let eng = GitEngine::open(dir.path()).unwrap();
    let refname = eng.branch(&GitCaveats::top(), "feat/x").unwrap();
    assert_eq!(refname, "refs/heads/feat/x");
    let ok = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-parse", "--verify", "refs/heads/feat/x"])
        .status()
        .unwrap()
        .success();
    assert!(ok, "branch ref must resolve under the system git too");
}
#[test]
fn writes_fail_closed_without_capability() {
    let dir = repo_with_commit();
    std::fs::write(dir.path().join("new.txt"), "x\n").unwrap();
    let eng = GitEngine::open(dir.path()).unwrap();
    let author = Author {
        name: "B".into(),
        email: "b@b".into(),
    };

    let ro = GitCaveats::read_only();
    assert!(matches!(
        eng.add(&ro, &["new.txt".to_string()]),
        Err(GitError::Denied("stage"))
    ));
    assert!(matches!(
        eng.commit(&ro, "m", &author),
        Err(GitError::Denied("commit"))
    ));
    assert!(matches!(
        eng.branch(&ro, "x"),
        Err(GitError::Denied("refs"))
    ));

    // Stage-but-not-commit: add is allowed, commit is refused.
    let stage_only = GitCaveats {
        commit_local: false,
        ..GitCaveats::top()
    };
    assert!(eng.add(&stage_only, &["new.txt".to_string()]).is_ok());
    assert!(matches!(
        eng.commit(&stage_only, "m", &author),
        Err(GitError::Denied("commit"))
    ));
}
