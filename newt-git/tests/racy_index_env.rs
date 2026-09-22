//! The racy-clean guard must read the mtime of the index the engine actually
//! loaded. `load_index` honours `GIT_INDEX_FILE`, so the mtime has to as well:
//! judged against `.git/index` instead, a racily clean edit reads as clean and
//! checkout overwrites it. A test binary of its own, because setting
//! `GIT_INDEX_FILE` inside the shared unit-test process would redirect every
//! other test's index.
use newt_core::caveats::Scope;
use newt_core::git_caveats::GitCaveats;
use newt_git::{GitEngine, GitError};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn set_mtime(path: &Path, when: std::time::SystemTime) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(when).unwrap();
}

#[test]
fn racy_check_reads_the_index_named_by_git_index_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q", "-b", "main"]);
    std::fs::write(p.join("a.txt"), "hello\n").unwrap();
    git(p, &["add", "a.txt"]);
    git(p, &["commit", "-q", "-m", "c1"]);
    git(p, &["checkout", "-q", "-b", "ahead"]);
    std::fs::write(p.join("a.txt"), "v2\n").unwrap();
    git(p, &["commit", "-q", "-am", "c2"]);
    git(p, &["checkout", "-q", "main"]);

    // From here on the engine loads this copy, not `.git/index`.
    let alt = p.join(".git/alt-index");
    std::fs::copy(p.join(".git/index"), &alt).unwrap();
    std::env::set_var("GIT_INDEX_FILE", &alt);

    // A same-size edit, re-cached in the ALT index against the old blob and
    // stamped in the same instant: racily clean in the index that counts.
    std::fs::write(p.join("a.txt"), "dirty\n").unwrap();
    // Backdated, so writing the index never smudges the entry as racy itself.
    let edited = std::time::SystemTime::now() - Duration::from_secs(10);
    set_mtime(&p.join("a.txt"), edited);
    let repo = grit_lib::repo::Repository::open(&p.join(".git"), Some(p)).unwrap();
    let mut index = repo.load_index().unwrap();
    let entry = index.get_mut(b"a.txt", 0).unwrap();
    *entry = grit_lib::index::entry_from_stat(&p.join("a.txt"), b"a.txt", entry.oid, entry.mode)
        .unwrap();
    // grit's `write_index` targets `.git/index` even when GIT_INDEX_FILE is
    // set (only `load_index` honours it), so write the alt index by path.
    repo.write_index_at(&alt, &mut index).unwrap();
    set_mtime(&alt, edited);
    // `.git/index` looks newer than the edit, so judging racy-ness against it
    // calls the entry settled and trusts its stale stat.
    set_mtime(&p.join(".git/index"), edited + Duration::from_secs(86_400));

    let eng = GitEngine::open(p, &Scope::All).unwrap();
    let err = eng
        .checkout(&GitCaveats::top(), "ahead", false)
        .unwrap_err();
    assert!(matches!(err, GitError::Refused(_)), "{err}");
    assert_eq!(std::fs::read_to_string(p.join("a.txt")).unwrap(), "dirty\n");
}
