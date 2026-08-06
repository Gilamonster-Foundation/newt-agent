//! Process-level coverage for `newt web` — the find-and-spawn launcher for
//! the workspace-excluded newt-web cockpit (decision D1).

// Every test here is `#[cfg(unix)]` (the stub web binary is a shell script), so
// on Windows these imports are unused and `-D unused-imports` fails the build.
// Gate the imports to match their only users.
#[cfg(unix)]
use assert_cmd::Command;
#[cfg(unix)]
use predicates::prelude::*;

/// A stub "newt-web" that records its argv and exits 0, so the launcher's
/// spawn/passthrough contract is provable without building the real crate.
#[cfg(unix)]
fn stub_web_binary(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join("newt-web");
    std::fs::write(&path, "#!/bin/sh\necho \"stub-newt-web:$@\"\nexit 0\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
#[test]
fn web_launches_the_env_override_binary_and_passes_args_through() {
    let dir = tempfile::tempdir().unwrap();
    let stub = stub_web_binary(dir.path());

    Command::cargo_bin("newt")
        .unwrap()
        .env("NEWT_WEB_BIN", &stub)
        .args(["web", "--port", "9999"])
        .assert()
        .success()
        .stdout(predicate::str::contains("stub-newt-web:--port 9999"));
}

#[cfg(unix)]
#[test]
fn web_missing_binary_error_names_every_escape_hatch() {
    let dir = tempfile::tempdir().unwrap();

    Command::cargo_bin("newt")
        .unwrap()
        // Point the override at nothing and empty the PATH so neither
        // fallback can accidentally find a real newt-web on the host.
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

#[cfg(unix)]
#[test]
fn web_propagates_a_nonzero_exit_code() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("newt-web");
    std::fs::write(&path, "#!/bin/sh\nexit 3\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

    Command::cargo_bin("newt")
        .unwrap()
        .env("NEWT_WEB_BIN", &path)
        .arg("web")
        .assert()
        .code(3);
}
