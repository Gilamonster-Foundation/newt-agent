//! Process-level coverage for `newt web` — the find-and-spawn launcher for
//! the workspace-excluded newt-web cockpit (decision D1).
//!
//! Unix-only as a WHOLE FILE: the stub binaries are shell scripts, and a
//! per-test `#[cfg(unix)]` left the imports below unused on Windows, where
//! `-D warnings` fails the clippy step (caught by Windows CI, invisible on
//! unix hosts where the imports are used).
#![cfg(unix)]

mod common;

use assert_cmd::Command;
use predicates::prelude::*;
use std::time::Duration;

/// Give the real launcher its own installation directory. A symlink would
/// resolve back to the shared Cargo target and retain its real web sibling.
fn isolated_newt_binary(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("newt");
    std::fs::copy(assert_cmd::cargo::cargo_bin("newt"), &path).unwrap();
    path
}

/// A stub "newt-web" that records its argv and exits 0, so the launcher's
/// spawn/passthrough contract is provable without building the real crate.
fn stub_web_binary(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("newt-web");
    std::fs::write(&path, "#!/bin/sh\necho \"stub-newt-web:$@\"\nexit 0\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn web_launches_the_env_override_binary_and_passes_args_through() {
    let dir = tempfile::tempdir().unwrap();
    let stub = stub_web_binary(dir.path());

    common::newt()
        .env("NEWT_WEB_BIN", &stub)
        .args(["web", "--port", "9999"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stub-newt-web:--port 9999"));
}

#[test]
fn web_missing_binary_error_names_every_escape_hatch() {
    let dir = tempfile::tempdir().unwrap();
    let newt = isolated_newt_binary(dir.path());

    let root = common::isolated_root();
    let mut cmd = Command::new(&newt);
    common::isolate(&mut cmd, root.path());
    cmd.timeout(Duration::from_secs(30))
        // Isolate all three lookup sources: absent override, no sibling,
        // and an empty installation directory as PATH. The shared target may
        // contain a real newt-web without changing this missing-binary probe.
        .env("NEWT_WEB_BIN", dir.path().join("absent-newt-web"))
        .env("PATH", dir.path())
        .arg("web")
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("just install-web")
                .and(predicate::str::contains("NEWT_WEB_BIN"))
                .and(predicate::str::contains(
                    "--manifest-path newt-web/Cargo.toml",
                )),
        );
}

#[test]
fn web_finds_the_isolated_sibling_when_override_and_path_are_absent() {
    let dir = tempfile::tempdir().unwrap();
    let newt = isolated_newt_binary(dir.path());
    stub_web_binary(dir.path());

    let root = common::isolated_root();
    let mut cmd = Command::new(&newt);
    common::isolate(&mut cmd, root.path());
    cmd.timeout(Duration::from_secs(30))
        .env("NEWT_WEB_BIN", dir.path().join("absent-newt-web"))
        .env("PATH", "")
        .args(["web", "--port", "9999"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stub-newt-web:--port 9999"));
}

#[test]
fn web_propagates_a_nonzero_exit_code() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("newt-web");
    std::fs::write(&path, "#!/bin/sh\nexit 3\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

    common::newt()
        .env("NEWT_WEB_BIN", &path)
        .arg("web")
        .assert()
        .code(3);
}
