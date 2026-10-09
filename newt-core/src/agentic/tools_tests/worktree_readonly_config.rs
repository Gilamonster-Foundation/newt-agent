//! PR #2827: a config-selected verifier is a write-capable descendant, even
//! for oneline log. Real Git grounds the admission grammar's read-only claim.
use super::super::*;
use crate::agentic::tools::{
    disable_ocap_tests::{env_lock, EnvVar},
    tests::git_shell_grant::hermetic_git,
};
use crate::{worktree_adoption::WorktreeSession, ExecOutcome, Scope};
use std::{io::Write, os::unix::fs::PermissionsExt, process::Stdio};

fn git(root: &Path, args: &[&str]) -> String {
    let out = hermetic_git(root, root).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

// Git parses this signature and invokes the verifier; no valid key is needed.
fn signed_commit(root: &Path, tree: &str, message: &str) -> String {
    let commit = format!("tree {tree}\nauthor fixture <fixture@example.invalid> 1 +0000\ncommitter fixture <fixture@example.invalid> 1 +0000\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n ZmFrZQ==\n -----END PGP SIGNATURE-----\n\n{message}\n");
    let mut child = hermetic_git(root, root)
        .args(["hash-object", "-t", "commit", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(commit.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn fixture() -> (tempfile::TempDir, PathBuf, Caveats) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("main");
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    for dir in [&root, &sub] {
        git(dir, &["init", "-q", "-b", "main"]);
    }
    let tree = git(&sub, &["mktree"]);
    let old = signed_commit(&sub, &tree, "before");
    let new = signed_commit(&sub, &tree, "after");
    git(&sub, &["update-ref", "refs/heads/main", &new]);
    std::fs::write(
        root.join(".gitmodules"),
        "[submodule \"sub\"]\n path = sub\n url = ./unused\n",
    )
    .unwrap();
    std::fs::write(root.join(".gitattributes"), "*.txt filter=hostile\n").unwrap();
    std::fs::write(root.join("file.txt"), "before\n").unwrap();
    git(&root, &["add", ".gitmodules", ".gitattributes", "file.txt"]);
    git(
        &root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{old},sub"),
        ],
    );
    let oid = signed_commit(&root, &git(&root, &["write-tree"]), "fixture");
    git(&root, &["update-ref", "refs/heads/main", &oid]);
    let helper = temp.path().join("hostile.sh");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\nprintf invoked > {}\nexit 1\n",
            crate::mcp::shell_quote_arg(root.join("marker").to_str().unwrap())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (key, value) in [
        ("gpg.program", helper.to_str().unwrap()),
        ("gpg.openpgp.program", helper.to_str().unwrap()),
        ("gpg.x509.program", helper.to_str().unwrap()),
        ("gpg.ssh.program", helper.to_str().unwrap()),
        ("log.showSignature", "true"),
        ("core.fsmonitor", helper.to_str().unwrap()),
        ("filter.hostile.clean", helper.to_str().unwrap()),
        ("core.untrackedCache", "true"),
        ("status.submoduleSummary", "true"),
        ("pager.status", helper.to_str().unwrap()),
    ] {
        for dir in [&root, &sub] {
            git(dir, &["config", key, value]);
        }
    }
    std::fs::write(root.join("file.txt"), "after!\n").unwrap();
    std::fs::File::open(root.join("file.txt"))
        .unwrap()
        .set_times(
            std::fs::FileTimes::new().set_modified(
                std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000),
            ),
        )
        .unwrap();
    let caveats = Caveats {
        fs_write: Scope::only([temp.path().to_string_lossy().into_owned()]),
        net: Scope::none(),
        ..Caveats::top()
    };
    (temp, root, caveats)
}

async fn dispatch(
    root: &Path,
    caveats: &Caveats,
    session: &WorktreeSession,
    command: &str,
) -> (String, Option<ExecOutcome>) {
    super::super::round2_tests::dispatch(
        serde_json::json!({"command":command}),
        root,
        caveats,
        session,
        None,
    )
    .await
}

/// PR #2827 P1: the authority control proves the verifier really can overwrite
/// the original marker. Creation with log/status must refuse BEFORE that happens.
#[tokio::test]
async fn readonly_git_hostile_log_and_status_refused() {
    let _lock = env_lock().await;
    let _paths = EnvVar::set("NEWT_EXEC_PATHS", "/usr/bin:/bin");
    let _venv = EnvVar::unset("NEWT_VENV");
    let _virtual = EnvVar::unset("VIRTUAL_ENV");
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    for log in [
        "git log --oneline -1",
        "git log --oneline -n 1",
        "git status",
        "git status --short",
        "git status -s",
        "git status --porcelain",
    ] {
        let (temp, root, caveats) = fixture();
        let _tmp = EnvVar::set("NEWT_CHILD_TMPDIR", temp.path().to_str().unwrap());
        let session = WorktreeSession::default();
        let (control, _) = dispatch(&root, &caveats, &session, log).await;
        assert_eq!(
            std::fs::read_to_string(root.join("marker")).ok().as_deref(),
            Some("invoked"),
            "authority control: {control}"
        );
        std::fs::write(root.join("marker"), "untouched").unwrap();
        let (out, outcome) = dispatch(
            &root,
            &caveats,
            &session,
            &format!("git worktree add -b task ../task HEAD && {log}"),
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(root.join("marker")).unwrap(),
            "untouched",
            "verifier executed before adoption: {out}"
        );
        assert_eq!(outcome, Some(ExecOutcome::Denied), "{out}");
        assert!(session.snapshot().is_none());
        assert!(!temp.path().join("task").exists());
    }
}

/// PR #2827: all retained forms must adopt without invoking repo-selected
/// verifier/fsmonitor/pager/filter helpers, despite sufficient write/exec authority.
#[tokio::test]
async fn readonly_git_hostile_config_retained_forms() {
    let _lock = env_lock().await;
    let _paths = EnvVar::set("NEWT_EXEC_PATHS", "/usr/bin:/bin");
    let _venv = EnvVar::unset("NEWT_VENV");
    let _virtual = EnvVar::unset("VIRTUAL_ENV");
    let _engine = EnvVar::set("NEWT_SHELL_ENGINE", "brush");
    for sibling in [
        "git branch --show-current",
        "git rev-parse HEAD",
        "git rev-parse --abbrev-ref HEAD",
        "git rev-parse --show-toplevel",
        "git worktree list",
    ] {
        let (temp, root, caveats) = fixture();
        let _tmp = EnvVar::set("NEWT_CHILD_TMPDIR", temp.path().to_str().unwrap());
        let session = WorktreeSession::default();
        let (control, _) = dispatch(&root, &caveats, &session, "git log --oneline -1").await;
        assert_eq!(
            std::fs::read_to_string(root.join("marker")).ok().as_deref(),
            Some("invoked"),
            "{control}"
        );
        std::fs::write(root.join("marker"), "untouched").unwrap();
        let (out, outcome) = dispatch(
            &root,
            &caveats,
            &session,
            &format!("git worktree add -b task ../task HEAD && {sibling}"),
        )
        .await;
        assert_eq!(outcome, Some(ExecOutcome::Passed), "{sibling}: {out}");
        assert!(session.snapshot().is_some(), "{sibling}: {out}");
        assert_eq!(
            std::fs::read_to_string(root.join("marker")).unwrap(),
            "untouched",
            "{sibling}: {out}"
        );
    }
}
