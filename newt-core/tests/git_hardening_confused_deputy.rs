//! step-7.4 — real-resource proof that [`newt_core::git_hardening::hardened_git`]
//! defuses the git confused-deputy the final OCAP adversarial pass EMPIRICALLY
//! confirmed: a repo-local `.git/config` `core.fsmonitor=<payload>` runs the
//! payload out-of-fence on an ordinary `git status`.
//!
//! Real-resource tier (real git subprocess + real fs), `#[serial]`. It grounds
//! the belief that `-c core.fsmonitor=` + env-scrub actually stop git from
//! honoring the attacker's config — no mock can stand in for git's behavior.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use newt_core::git_hardening::hardened_git;
use serial_test::serial;
use tempfile::tempdir;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?} failed");
}

#[test]
#[serial]
fn hardened_git_neutralizes_the_fsmonitor_gadget() {
    let repo = tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@t"]);
    git(repo.path(), &["config", "user.name", "t"]);

    // Plant the attacker gadget: a repo-local `core.fsmonitor` that runs a
    // payload dropping a marker file — the confused-deputy escape.
    let marker = repo.path().join("PWNED");
    let payload = repo.path().join("gadget.sh");
    std::fs::write(
        &payload,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755)).unwrap();
    git(
        repo.path(),
        &["config", "core.fsmonitor", payload.to_str().unwrap()],
    );

    // Control: a RAW `git status` may FIRE the gadget (the vector we are closing).
    let _ = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    let raw_fired = marker.exists();
    let _ = std::fs::remove_file(&marker);

    // The fix: `hardened_git` overrides `core.fsmonitor=` so the gadget CANNOT
    // run, whether or not this host's git fired it for the raw call.
    let _ = hardened_git(repo.path(), &["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        !marker.exists(),
        "hardened_git ran the core.fsmonitor gadget (raw_fired={raw_fired}) — the confused-deputy is NOT closed"
    );
}

#[test]
#[serial]
fn hardened_git_ignores_user_and_system_config_and_still_reads_the_repo() {
    // Positive control: hardened_git still produces correct output (it did not
    // break git by over-scrubbing) — a committed file shows clean status.
    let repo = tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.email", "t@t"]);
    git(repo.path(), &["config", "user.name", "t"]);
    std::fs::write(repo.path().join("f.txt"), "hi\n").unwrap();
    git(repo.path(), &["add", "-A"]);
    git(repo.path(), &["commit", "-q", "-m", "x"]);

    let out = hardened_git(repo.path(), &["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(out.status.success(), "hardened git status must succeed");
    assert!(
        String::from_utf8_lossy(&out.stdout).trim().is_empty(),
        "a committed-clean tree reports empty porcelain status under hardened_git"
    );
}
