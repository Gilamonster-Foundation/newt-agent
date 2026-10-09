//! PR #2827: production Brush must enforce the same creation-sibling allowlist.
//! Grounds portable admission rows under the real kernel fence, including macOS.
use super::{dispatch, fixture, real_git_output, WorktreeSession};

pub async fn run() {
    for sibling in [
        "git status --short",
        "git status -s",
        "git status --porcelain",
        "git log --oneline -5",
    ] {
        let (_temp, main, task, caveats) = fixture();
        let session = WorktreeSession::default();
        let old = real_git_output(&main, &["rev-parse", "HEAD"]);
        let command = format!("git worktree add -b task ../task 2>&1 | tail -5 && {sibling}");
        let out = dispatch(&session, &main, &caveats, &command).await;
        assert!(out.contains("capability denied"), "{command}: {out}");
        assert!(!task.exists());
        assert!(!out.contains("Adopted task worktree:"));
        assert_eq!(real_git_output(&main, &["rev-parse", "HEAD"]), old);
        assert_eq!(
            std::fs::read_to_string(main.join("sentinel")).unwrap(),
            "keep me"
        );
        println!("PASS creation_sibling_refused {sibling}");
    }
    for (start, sibling, expected) in [
        (".", "git branch --show-current", "main"),
        ("main", "git rev-parse --abbrev-ref HEAD", "main"),
        ("SHA", "git branch --show-current", "main"),
        ("HEAD", "git rev-parse HEAD", "SHA"),
        ("HEAD", "git rev-parse --show-toplevel", "ROOT"),
        ("HEAD", "git worktree list", "[task]"),
    ] {
        let (_temp, main, task, caveats) = fixture();
        let session = WorktreeSession::default();
        let old = real_git_output(&main, &["rev-parse", "HEAD"]);
        let start = if start == "SHA" { &old } else { start };
        let expected = match expected {
            "SHA" => &old,
            "ROOT" => main.to_str().unwrap(),
            other => other,
        };
        let command =
            format!("git worktree add -b task ../task {start} 2>&1 | tail -5 && {sibling}");
        let out = dispatch(&session, &main, &caveats, &command).await;
        assert!(out.contains("Adopted task worktree:"), "{command}: {out}");
        assert!(out.contains(expected), "{command}: {out}");
        assert!(task.join(".git").is_file());
        assert_eq!(real_git_output(&task, &["rev-parse", "HEAD"]), old);
        assert_eq!(real_git_output(&main, &["rev-parse", "HEAD"]), old);
        assert_eq!(
            std::fs::read_to_string(main.join("sentinel")).unwrap(),
            "keep me"
        );
        let cwd = dispatch(&session, &main, &caveats, "pwd").await;
        assert!(cwd.contains(task.to_str().unwrap()), "{cwd}");
        println!("PASS creation_sibling_admitted {sibling} start={start}");
    }
}
