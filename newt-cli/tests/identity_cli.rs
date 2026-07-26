//! Integration tests for `newt identity`.
//!
//! Each test runs `newt` in a private working directory with `HOME` pointed at
//! a separate empty tempdir, so resolution never reads the developer's real
//! `~/.newt`. Because the working directory and `$HOME` are distinct empty
//! trees, the default case resolves cleanly to the compiled-in
//! `newt-agent` identity.

use assert_cmd::Command;
use predicates::prelude::*;

/// A `newt identity` command with `HOME` and cwd isolated to fresh tempdirs.
/// Returns the command plus the two tempdirs (kept alive for the test).
fn newt_identity_in(workspace: &std::path::Path, home: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("newt").unwrap();
    cmd.current_dir(workspace).env("HOME", home).arg("identity");
    cmd
}

#[test]
fn identity_default_resolves_to_newt_agent_user() {
    // Empty workspace, empty home: nothing on disk → the compiled-in default.
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    newt_identity_in(workspace.path(), home.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "309460085+newt-agent@users.noreply.github.com",
        ))
        .stdout(predicate::str::contains("compiled-in default (newt-agent)"))
        .stdout(predicate::str::contains("signing-key: <none>"))
        .stdout(predicate::str::contains("github-app:  <none>"))
        .stdout(predicate::str::contains("tokens:      <none>"))
        .stdout(predicate::str::contains("newt-agent").and(predicate::str::contains("[bot]").not()))
        .stdout(predicate::str::contains("newt identity set"));
}

#[test]
fn identity_set_writes_home_override_and_show_picks_it_up() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let mut set = Command::cargo_bin("newt").unwrap();
    set.current_dir(workspace.path())
        .env("HOME", home.path())
        .args([
            "identity",
            "set",
            "--name",
            "my-harness",
            "--email",
            "my-harness@example.com",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Wrote agent identity"))
        .stdout(predicate::str::contains("my-harness@example.com"));

    let written = home.path().join(".newt").join("agent-identity.toml");
    assert!(
        written.is_file(),
        "set must land in ~/.newt/agent-identity.toml"
    );
    let body = std::fs::read_to_string(&written).unwrap();
    assert!(body.contains("[agent-identity]"));
    assert!(body.contains("my-harness"));

    newt_identity_in(workspace.path(), home.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("my-harness"))
        .stdout(predicate::str::contains("my-harness@example.com"))
        .stdout(predicate::str::contains("home"))
        .stdout(predicate::str::contains("newt identity set").not());
}

#[test]
fn identity_set_workspace_writes_local_override() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let mut set = Command::cargo_bin("newt").unwrap();
    set.current_dir(workspace.path())
        .env("HOME", home.path())
        .args([
            "identity",
            "set",
            "--name",
            "ws-agent",
            "--email",
            "ws@example.com",
            "--workspace",
        ])
        .assert()
        .success();

    let written = workspace.path().join(".newt").join("agent-identity.toml");
    assert!(written.is_file());

    newt_identity_in(workspace.path(), home.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("ws-agent"))
        .stdout(predicate::str::contains("ws@example.com"))
        .stdout(predicate::str::contains("workspace"));
}

#[test]
fn identity_workspace_file_overrides_default_and_hides_secret() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let newt_dir = workspace.path().join(".newt");
    std::fs::create_dir_all(&newt_dir).unwrap();
    std::fs::write(
        newt_dir.join("agent-identity.toml"),
        r#"
[agent-identity]
name = "gilamonster-agent[bot]"
email = "293450354+gilamonster-agent[bot]@users.noreply.github.com"

[agent-identity.github_app]
app_id = 4046825
client_id = "Iv23li5iPGv4awNHpHbZ"
installation_id = 140120359

[agent-identity.tokens]
ci_deploy = { env = "NEWT_IDENTITY_CLI_SECRET" }
"#,
    )
    .unwrap();

    newt_identity_in(workspace.path(), home.path())
        // A token VALUE is present in the env; it must never reach stdout.
        .env("NEWT_IDENTITY_CLI_SECRET", "SECRET-MUST-NOT-PRINT")
        .assert()
        .success()
        .stdout(predicate::str::contains("gilamonster-agent[bot]"))
        .stdout(predicate::str::contains(
            "293450354+gilamonster-agent[bot]@users.noreply.github.com",
        ))
        .stdout(predicate::str::contains("workspace"))
        // GitHub App public coordinates show.
        .stdout(predicate::str::contains("app_id:          4046825"))
        .stdout(predicate::str::contains("installation_id: 140120359"))
        // Token NAME shows...
        .stdout(predicate::str::contains("ci_deploy"))
        .stdout(predicate::str::contains("names only"))
        // ...but the resolved token VALUE never does.
        .stdout(predicate::str::contains("SECRET-MUST-NOT-PRINT").not());
}

#[test]
fn identity_missing_signing_key_is_clean_not_panic() {
    // A configured signing-key path that doesn't exist must render an
    // "<unavailable>" note and still exit 0 — never panic, never mint.
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let newt_dir = workspace.path().join(".newt");
    std::fs::create_dir_all(&newt_dir).unwrap();
    std::fs::write(
        newt_dir.join("agent-identity.toml"),
        r#"
[agent-identity]
name = "keyed-agent[bot]"
signing_key = "/nonexistent/vault/path/identity.pem"
"#,
    )
    .unwrap();

    newt_identity_in(workspace.path(), home.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "/nonexistent/vault/path/identity.pem",
        ))
        .stdout(predicate::str::contains("<unavailable"));
}
